//! Boards synced with Linear or Jira ([`crate::board_sync`]) as the desk
//! runs them: the hubs a board can sync through, linking and unlinking a
//! board, pausing it. A board's link belongs to its group: it imports only
//! its own project's or team's issues, onto itself, and is paused when it
//! leaves the group it was linked in.

use std::collections::BTreeMap;

use serde::Deserialize;
use uuid::Uuid;

use serde_json::json;

use super::{Desk, DeskError};
use crate::board::{
    self, Board, External, Filter, Paused, Retry, Seen, Sync, System, Ticket, TicketState, Via,
};
use crate::board_sync::{check_filter, check_target, Issue, Remote, RemoteError};
use crate::board_sync_store::SyncHub;
use crate::crew::{By, Kind};
use crate::model::DeskEvent;

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

/// A link the person asks for, once the hub has said it offers the tools
/// (the hub's `desk_board_sync` checks that first).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkBoard {
    pub system: System,
    pub target: String,
    /// The Linear team's id, from the hub's own list of teams.
    #[serde(default)]
    pub target_id: Option<String>,
    pub via: Via,
    pub filter: Filter,
    /// The person's Linear user id, for "assigned to me" there.
    #[serde(default)]
    pub me: Option<String>,
    pub states: BTreeMap<TicketState, String>,
}

/// Whether `id` can name a hub here: lowercase letters, digits and `-`.
pub fn check_hub_id(id: &str) -> Result<(), DeskError> {
    let ok = (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-');
    match ok {
        true => Ok(()),
        false => Err(DeskError::Invalid(format!(
            "{id:?} can't name a hub: use lowercase letters, digits and dashes."
        ))),
    }
}

/// A hub's URL: `https://`, or `http://` to this computer only, with no
/// credentials, query or fragment in it.
pub fn check_hub_url(url: &str) -> Result<url::Url, DeskError> {
    let bad = || {
        DeskError::Invalid(format!(
            "{url:?} isn't a hub's address, like https://atlas.example."
        ))
    };
    let u = url::Url::parse(url.trim()).map_err(|_| bad())?;
    let local = matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let scheme_ok = u.scheme() == "https" || (u.scheme() == "http" && local);
    if !scheme_ok
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(u)
}

impl Desk {
    // ── Hubs ─────────────────────────────────────────────────────────────

    pub async fn sync_hubs(&self) -> Result<Vec<SyncHub>, DeskError> {
        self.0.store.sync_hubs().await.map_err(internal)
    }

    pub async fn sync_hub(&self, id: &str) -> Result<Option<SyncHub>, DeskError> {
        self.0.store.sync_hub(id).await.map_err(internal)
    }

    /// Keeps the hub `id` (named `name`, at `url`) as one boards may sync
    /// through as `person`. Its token is the caller's to keep, in the vault.
    pub async fn put_sync_hub(
        &self,
        id: &str,
        name: &str,
        url: &str,
        person: &str,
    ) -> Result<SyncHub, DeskError> {
        check_hub_id(id)?;
        let url = check_hub_url(url)?;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(DeskError::Invalid(
                "A hub needs a name, on one line.".into(),
            ));
        }
        let at = crate::crew::now();
        let created_at = self.sync_hub(id).await?.map_or(at, |h| h.created_at);
        let h = SyncHub {
            id: id.to_string(),
            name: name.to_string(),
            url: url.as_str().trim_end_matches('/').to_string(),
            person: person.to_string(),
            created_at,
            updated_at: at,
        };
        self.0.store.put_sync_hub(&h).await.map_err(internal)?;
        // A board paused for want of this hub, or of a token for it, can
        // try again.
        for b in self.boards().await? {
            let waits = b.sync.as_ref().is_some_and(|s| {
                s.via
                    == (Via::Hub {
                        hub: id.to_string(),
                    })
                    && matches!(s.paused, Some(Paused::HubUnpaired | Paused::NoToken))
            });
            if waits {
                self.set_sync_paused(b.id, None).await?;
            }
        }
        Ok(h)
    }

    /// Forgets the hub `id`: the boards that sync through it pause until
    /// they are linked through another, or kept here unsynced.
    pub async fn forget_sync_hub(&self, id: &str) -> Result<(), DeskError> {
        if !self.0.store.delete_sync_hub(id).await.map_err(internal)? {
            return Err(DeskError::NotFound);
        }
        for b in self.boards().await? {
            if b.sync.as_ref().is_some_and(|s| {
                s.via
                    == (Via::Hub {
                        hub: id.to_string(),
                    })
            }) {
                self.set_sync_paused(b.id, Some(Paused::HubUnpaired))
                    .await?;
            }
        }
        Ok(())
    }

    // ── Links ────────────────────────────────────────────────────────────

    /// Links board `id` to `link`'s project or team, for `person`. A board
    /// has one link; a project or team is linked to one board.
    pub async fn link_board(
        &self,
        id: Uuid,
        link: LinkBoard,
        person: &str,
    ) -> Result<Board, DeskError> {
        let b = self.board_of(id).await?;
        check_target(link.system, &link.target).map_err(DeskError::Invalid)?;
        check_filter(link.system, &link.filter, link.me.as_deref()).map_err(DeskError::Invalid)?;
        if link.system == System::Linear {
            let id = link.target_id.as_deref().ok_or_else(|| {
                DeskError::Invalid(
                    "Say which Linear team, by its id from the hub's list of teams.".into(),
                )
            })?;
            crate::board_sync::check_linear_id(id).map_err(DeskError::Invalid)?;
        }
        if let Via::Hub { hub } = &link.via {
            check_hub_id(hub)?;
            if self.sync_hub(hub).await?.is_none() {
                return Err(DeskError::Invalid(format!(
                    "There's no hub {hub} linked here."
                )));
            }
        }
        if b.sync.is_some() {
            return Err(DeskError::Conflict(format!(
                "{} is linked already; unlink it first.",
                b.name
            )));
        }
        if let Some(other) = self
            .0
            .store
            .board_linked_to(link.system, &link.target, &link.via, id)
            .await
            .map_err(internal)?
        {
            let other = self.board_of(other).await?;
            return Err(DeskError::Conflict(format!(
                "{} {} is linked to the board {} already.",
                link.system.title(),
                link.target,
                other.name
            )));
        }
        if !self
            .0
            .store
            .board_tickets(id)
            .await
            .map_err(internal)?
            .is_empty()
        {
            return Err(DeskError::Conflict(format!(
                "{} has tickets of its own; link an empty board, or make a new one for {}.",
                b.name, link.target
            )));
        }
        let sync = Sync {
            link: Uuid::new_v4(),
            system: link.system,
            target: link.target,
            target_id: link.target_id,
            via: link.via,
            person: person.to_string(),
            filter: link.filter,
            me: link.me,
            states: link.states,
            group: b.group.clone(),
            cursor: None,
            paused: None,
            pulled_at: None,
            swept_at: None,
            backoff_until: None,
            failures: 0,
            problem: None,
            retry: Default::default(),
        };
        self.0
            .store
            .change_board_sync(id, move |_, _| Ok((Some(sync), None)))
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        self.board_announced(id).await
    }

    /// Unlinks board `id`: it keeps its tickets, unsynced, with their
    /// issues' keys, and takes a prefix of its own again for new ones.
    pub async fn unlink_board(&self, id: Uuid) -> Result<Board, DeskError> {
        let b = self.board_of(id).await?;
        if b.sync.is_none() {
            return Ok(b);
        }
        let taken: std::collections::HashSet<String> = self
            .boards()
            .await?
            .into_iter()
            .filter_map(|b| b.prefix)
            .collect();
        let base = board::prefix_of(&b.name);
        let prefix = std::iter::once(base.clone())
            .chain((2..100).map(|n| format!("{}{n}", &base[..base.len().min(4)])))
            .find(|p| !taken.contains(p) && board::check_prefix(p).is_ok())
            .ok_or_else(|| DeskError::Conflict("No prefix is free for this board.".into()))?;
        self.0
            .store
            .change_board_sync(id, move |_, old| Ok((None, old.or(Some(prefix)))))
            .await
            .map_err(internal)?;
        for mut t in self.0.store.board_tickets(id).await.map_err(internal)? {
            if let Some(e) = t.external.as_mut().filter(|e| e.synced) {
                e.synced = false;
                if self.save_ticket(&mut t).await.is_ok() {
                    if let Ok(v) = self.view_ticket(t).await {
                        self.announce(DeskEvent::Ticket {
                            ticket: Box::new(v),
                        });
                    }
                }
            }
        }
        self.board_announced(id).await
    }

    /// The person pauses (or resumes) board `id`'s sync, in both
    /// directions. Resuming clears whatever paused it: the next pass finds
    /// out again.
    pub async fn pause_board(&self, id: Uuid, paused: bool) -> Result<Board, DeskError> {
        let b = self.board_of(id).await?;
        if b.sync.is_none() {
            return Err(DeskError::Invalid(format!("{} isn't synced.", b.name)));
        }
        self.set_sync_paused(id, paused.then_some(Paused::ByYou))
            .await?;
        self.board_announced(id).await
    }

    /// Sets why board `id` is paused (none: it isn't).
    pub async fn set_sync_paused(&self, id: Uuid, paused: Option<Paused>) -> Result<(), DeskError> {
        self.change_sync(id, |s| {
            s.paused = paused;
            if s.paused.is_none() {
                s.backoff_until = None;
                s.failures = 0;
            }
        })
        .await
        .map(|_| ())
    }

    /// Changes board `id`'s link with `f`, announcing the board; `None`
    /// when it has none.
    pub async fn change_sync(
        &self,
        id: Uuid,
        f: impl FnOnce(&mut Sync) + Send,
    ) -> Result<Option<Sync>, DeskError> {
        let changed = self
            .0
            .store
            .change_board_sync(id, move |sync, prefix| {
                Ok((
                    sync.map(|mut s| {
                        f(&mut s);
                        s
                    }),
                    prefix,
                ))
            })
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        let _ = self.board_announced(id).await;
        Ok(changed.0)
    }

    pub(super) async fn board_announced(&self, id: Uuid) -> Result<Board, DeskError> {
        let b = self.board_of(id).await?;
        self.announce(DeskEvent::Board {
            board: Box::new(b.clone()),
        });
        Ok(b)
    }

    /// Board `id`'s link as the loop runs it, with the time now.
    pub async fn board_sync(&self, id: Uuid) -> Result<Option<(Board, Sync)>, DeskError> {
        let b = self.board_of(id).await?;
        Ok(b.sync.clone().map(|s| (b, s)))
    }

    /// Every synced board's id.
    pub async fn synced_boards(&self) -> Result<Vec<Uuid>, DeskError> {
        self.0.store.synced_boards().await.map_err(internal)
    }

    /// Whether a synced board left the group it was linked in: it then
    /// doesn't sync until linked again, so no group's issues reach another.
    pub fn left_its_group(b: &Board) -> bool {
        b.sync.as_ref().is_some_and(|s| s.group != b.group)
    }
}

// ── The pull ─────────────────────────────────────────────────────────────

/// What one pass of a board's pull did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pulled {
    pub imported: usize,
    pub updated: usize,
    pub detached: usize,
    pub dropped: usize,
    /// Why it didn't run, when it didn't.
    pub skipped: Option<String>,
}

/// The calls one pass may still make.
struct Budget(usize);

impl Budget {
    fn take(&mut self) -> Result<(), RemoteError> {
        match self.0.checked_sub(1) {
            Some(left) => {
                self.0 = left;
                Ok(())
            }
            None => Err(RemoteError::Spent),
        }
    }
}

/// The issue `key` failed again: tried later, on its own back-off.
fn failed_again(
    retry: &mut BTreeMap<String, Retry>,
    key: &str,
    why: String,
    now: chrono::DateTime<chrono::Utc>,
) {
    let tries = retry.get(key).map_or(0, |r| r.tries) + 1;
    retry.insert(
        key.to_string(),
        Retry {
            tries,
            next_at: now + crate::board_sync::backoff(tries),
            problem: why.chars().take(300).collect(),
        },
    );
}

/// Whether a sync's pause is one a pass tries past once its back-off is
/// over: the hub was away or said no, which may have changed.
fn retried(p: &Paused) -> bool {
    matches!(
        p,
        Paused::HubOffline { .. }
            | Paused::NoToken
            | Paused::Missing { .. }
            | Paused::Refused { .. }
    )
}

/// What stops board `b`'s sync now, in both directions, if anything.
fn held(b: &Board, s: &Sync, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    if Desk::left_its_group(b) {
        return Some("it left the group it was linked in".into());
    }
    match &s.paused {
        Some(p) if !retried(p) => return Some(format!("paused: {p:?}")),
        _ => {}
    }
    match s.backoff_until {
        Some(until) if until > now => Some("backing off".into()),
        _ => None,
    }
}

impl Desk {
    /// Board `id`'s sync went wrong with `e`: paused when the hub is away or
    /// refused, backing off either way.
    pub async fn sync_failed(&self, id: Uuid, e: &RemoteError) -> Result<(), DeskError> {
        let now = crate::crew::now();
        self.change_sync(id, |s| {
            s.failures = s.failures.saturating_add(1);
            let wait = match e {
                RemoteError::RateLimited {
                    retry_after_secs: Some(secs),
                } => chrono::Duration::seconds((*secs).min(3600) as i64),
                _ => crate::board_sync::backoff(s.failures),
            };
            s.backoff_until = Some(now + wait);
            s.problem = Some(e.to_string().chars().take(300).collect());
            let paused = match e {
                RemoteError::Offline(_) => Some(Paused::HubOffline {
                    since: match &s.paused {
                        Some(Paused::HubOffline { since }) => *since,
                        _ => now,
                    },
                }),
                RemoteError::NoToken => Some(Paused::NoToken),
                RemoteError::Refused(reason) => Some(Paused::Refused {
                    reason: reason.clone(),
                }),
                _ => None,
            };
            if paused.is_some() {
                s.paused = paused;
            }
        })
        .await
        .map(drop)
    }

    /// Board `id`'s sync is going again: any pause the hub caused is over.
    async fn sync_ok(
        &self,
        id: Uuid,
        cursor: Option<String>,
        swept: bool,
        retry: BTreeMap<String, Retry>,
    ) -> Result<(), DeskError> {
        let now = crate::crew::now();
        self.change_sync(id, |s| {
            s.retry = retry;
            if s.paused.as_ref().is_some_and(retried) {
                s.paused = None;
            }
            s.failures = 0;
            s.backoff_until = None;
            s.problem = None;
            s.pulled_at = Some(now);
            if swept {
                s.swept_at = Some(now);
            }
            if cursor.is_some() {
                s.cursor = cursor;
            }
        })
        .await
        .map(drop)
    }

    /// Pulls board `id`'s issues from `remote`: the ones that changed since
    /// its cursor, imported or updated; and, every [`SWEEP_EVERY`], which of
    /// its tickets' issues left the filter or the project. Only issues in the
    /// board's own project or team are ever taken, onto this board alone,
    /// and only tickets this link imported are changed.
    pub async fn pull_board(&self, id: Uuid, remote: &dyn Remote) -> Result<Pulled, DeskError> {
        let now = crate::crew::now();
        let Some((b, s)) = self.board_sync(id).await? else {
            return Ok(Pulled {
                skipped: Some("not synced".into()),
                ..Default::default()
            });
        };
        if Desk::left_its_group(&b) && s.paused != Some(Paused::LeftGroup) {
            self.set_sync_paused(id, Some(Paused::LeftGroup)).await?;
        }
        if let Some(why) = held(&b, &s, now) {
            return Ok(Pulled {
                skipped: Some(why),
                ..Default::default()
            });
        }
        let mut budget = Budget(crate::board_sync::CALLS_PER_PASS);
        let sweep = s
            .swept_at
            .is_none_or(|at| now - at >= crate::board_sync::SWEEP_EVERY);
        let mut out = Pulled::default();
        let mut retry = s.retry.clone();
        match self
            .pull_pass(&b, &s, remote, &mut budget, sweep, &mut retry, &mut out)
            .await
        {
            Ok((cursor, swept)) => {
                self.sync_ok(id, cursor, swept, retry).await?;
                Ok(out)
            }
            Err(e) => {
                self.sync_failed(id, &e).await?;
                Ok(Pulled {
                    skipped: Some(e.to_string()),
                    ..out
                })
            }
        }
    }

    /// The issues `s`'s filter matches since `since` (all, without), oldest
    /// change first, in the board's project or team only; and whether that
    /// is all of them (no page was left unread).
    async fn matching(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        since: Option<&str>,
        budget: &mut Budget,
    ) -> Result<(Vec<Issue>, bool), RemoteError> {
        let mut issues = vec![];
        let mut complete = true;
        match s.system {
            System::Jira => {
                let mut page: Option<String> = None;
                for n in 0.. {
                    if n == crate::board_sync::PAGES_PER_PASS {
                        complete = false;
                        break;
                    }
                    if budget.take().is_err() {
                        complete = false;
                        break;
                    }
                    let mut args = json!({
                        "jql": crate::board_sync::jql(&s.target, &s.filter, since),
                        "max_results": 100,
                    });
                    if let Some(p) = &page {
                        args["next_page_token"] = json!(p);
                    }
                    let answer =
                        crate::board_sync::call(remote, s.system, "jira.search", args).await?;
                    let (found, next) = crate::board_sync::jira_search_issues(&answer, "");
                    issues.extend(found);
                    match next {
                        Some(n) => page = Some(n),
                        None => break,
                    }
                }
            }
            System::Linear => {
                if budget.take().is_err() {
                    return Ok((vec![], false));
                }
                let team = s.target_id.as_deref().unwrap_or_default();
                let answer = crate::board_sync::call(
                    remote,
                    s.system,
                    "linear.issue_list",
                    crate::board_sync::linear_args(team, &s.filter, s.me.as_deref()),
                )
                .await?;
                let found = crate::board_sync::linear_issues(&answer);
                // One page of 100: more than that may be left unread.
                complete = found.len() < 100;
                let cursor = since.and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok());
                issues.extend(
                    found
                        .into_iter()
                        .filter(|i| match &s.filter {
                            Filter::Label(l) => {
                                i.labels.iter().any(|x| x.eq_ignore_ascii_case(l.trim()))
                            }
                            _ => true,
                        })
                        .filter(|i| cursor.is_none_or(|c| i.updated_at >= c)),
                );
            }
        }
        issues.retain(|i| crate::board_sync::key_in_target(&i.key, &s.target));
        issues.sort_by_key(|i| i.updated_at);
        Ok((issues, complete))
    }

    /// One pull: what changed, then (when `sweep`) who left. The new
    /// cursor, when every changed issue was taken in.
    async fn pull_pass(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        sweep: bool,
        retry: &mut BTreeMap<String, Retry>,
        out: &mut Pulled,
    ) -> Result<(Option<String>, bool), RemoteError> {
        let now = crate::crew::now();
        // What failed before, when its turn comes.
        let due: Vec<String> = retry
            .iter()
            .filter(|(_, r)| r.next_at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in due {
            let taken = match self.fetch_issue(s, remote, budget, &key).await {
                Ok(Some(issue)) => self.take_in(b, s, remote, budget, &issue, out).await,
                // Gone from the project: the sweep sees to its ticket.
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            match taken {
                Ok(()) => {
                    retry.remove(&key);
                }
                Err(RemoteError::Failed(why)) => failed_again(retry, &key, why, now),
                Err(RemoteError::Spent) => return Ok((None, false)),
                Err(e) => return Err(e),
            }
        }
        let (changed, complete) = self
            .matching(s, remote, s.cursor.as_deref(), budget)
            .await?;
        let mut cursor = None;
        for issue in changed {
            match self.take_in(b, s, remote, budget, &issue, out).await {
                Ok(()) => {}
                // A tool that failed on this one issue: it is tried again on
                // its own back-off, and the cursor goes on past it.
                Err(RemoteError::Failed(why)) => {
                    tracing::info!(board = %b.id, issue = %issue.key, error = %why, "an issue wasn't pulled");
                    failed_again(retry, &issue.key, why, now);
                }
                // The pass's calls are spent: what was taken in stands.
                Err(RemoteError::Spent) => return Ok((cursor, false)),
                Err(e) => return Err(e),
            }
            cursor = Some(issue.updated_at.to_rfc3339());
        }
        if !complete {
            return Ok((cursor, false));
        }
        if sweep {
            match self.sweep(b, s, remote, budget, out).await {
                Ok(()) => return Ok((cursor, true)),
                Err(RemoteError::Spent) => return Ok((cursor, false)),
                Err(e) => return Err(e),
            }
        }
        Ok((cursor, false))
    }

    /// The issue `key`, read whole; `None` when it isn't in the board's
    /// project or team any more.
    async fn fetch_issue(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        key: &str,
    ) -> Result<Option<Issue>, RemoteError> {
        budget.take()?;
        let (tool, args) = match s.system {
            System::Jira => ("jira.get_issue", json!({ "issue_key": key })),
            System::Linear => ("linear.issue_get", json!({ "identifier": key })),
        };
        let v = crate::board_sync::call(remote, s.system, tool, args).await?;
        let issue = match s.system {
            System::Jira => crate::board_sync::jira_issue(&v, ""),
            System::Linear => crate::board_sync::linear_issue(v.get("issue").unwrap_or(&v)),
        };
        Ok(issue.filter(|i| crate::board_sync::key_in_target(&i.key, &s.target)))
    }

    /// Takes `issue` in: updates its ticket on this board, brings one back
    /// from the shelf, or imports it.
    async fn take_in(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        issue: &Issue,
        out: &mut Pulled,
    ) -> Result<(), RemoteError> {
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let existing = self
            .0
            .store
            .ticket_by_external(s.system, s.link, &issue.key)
            .await
            .map_err(|e| fail(internal(e)))?;
        if let Some(t) = &existing {
            let e = t.external.as_ref().expect("found by its issue");
            // Another board's, or another link's: never this one's to change.
            if t.board != b.id || e.link != s.link || e.kept {
                return Ok(());
            }
            if issue.updated_at <= e.remote_updated_at && e.synced {
                return Ok(());
            }
        }
        let issue = self.with_body(s, remote, budget, issue).await?;
        match existing {
            Some(t) => {
                if self.update_pulled(b, s, t, &issue).await.map_err(fail)? {
                    out.updated += 1;
                }
            }
            None => {
                if self.import(b, s, &issue).await.map_err(fail)? {
                    out.imported += 1;
                }
            }
        }
        Ok(())
    }

    /// `issue` with its description: a Jira search carries none, so the
    /// issue is read.
    async fn with_body(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        issue: &Issue,
    ) -> Result<Issue, RemoteError> {
        if issue.body.is_some() {
            return Ok(issue.clone());
        }
        budget.take()?;
        let full = crate::board_sync::call(
            remote,
            s.system,
            "jira.get_issue",
            json!({ "issue_key": issue.key }),
        )
        .await?;
        let read = crate::board_sync::jira_issue(&full, "")
            .filter(|i| i.key == issue.key)
            .ok_or_else(|| RemoteError::Failed(format!("{} couldn't be read", issue.key)))?;
        Ok(Issue {
            url: issue.url.clone(),
            ..read
        })
    }

    fn pulled_by(s: &Sync) -> By {
        By {
            party: crate::board_sync::remote_party(s.system, &s.target),
            reason: None,
            suggestion_id: None,
            hop: 0,
        }
    }

    /// Writes what `issue` changed on `t`. Whether it changed anything.
    async fn update_pulled(
        &self,
        b: &Board,
        s: &Sync,
        mut t: Ticket,
        issue: &Issue,
    ) -> Result<bool, DeskError> {
        let came_back = t.external.as_ref().is_some_and(|e| !e.synced);
        let p = crate::board_sync::pulled(&t, issue, &s.states);
        let at = crate::crew::now();
        let from = t.state;
        let mut said = vec![];
        if let Some(title) = &p.title {
            said.push(format!(
                "renamed it “{}”",
                crate::crew::one_line(title, 120)
            ));
            t.title = title.clone();
        }
        if let Some(body) = &p.body {
            said.push("edited its description".to_string());
            t.body = body.clone();
        }
        if let Some(state) = p.state {
            self.to_column(&mut t, state, at).await?;
            if crate::board::unassigns(state) {
                t.assignee = None;
            }
            said.push(format!("moved it to {}", state.title()));
        }
        if came_back {
            said.push("it matches the board's filter again".to_string());
        }
        let e = t.external.as_mut().expect("a synced ticket");
        e.remote_updated_at = e.remote_updated_at.max(issue.updated_at);
        e.synced = true;
        e.flagged = e.flagged || issue.flagged;
        if !issue.url.is_empty() {
            e.url = issue.url.clone();
        }
        // What it now says is what the issue says: a push never sends it back.
        e.seen.title = t.title.clone();
        e.seen.body = t.body.clone();
        if p.state.is_some() {
            e.seen.state = Some(t.state);
        }
        if said.is_empty() {
            self.save_ticket(&mut t).await?;
            return Ok(false);
        }
        t.updated_at = at;
        self.save_ticket(&mut t).await?;
        let key = t.key(b).unwrap_or_default();
        let by = Self::pulled_by(s);
        let text = format!("{} on {key}", said.join(" and "));
        let kind = match p.state {
            Some(_) if from != t.state => Kind::TicketMoved,
            _ => Kind::TicketEdited,
        };
        self.ticket_changed(t, &by, kind, None, text).await?;
        Ok(true)
    }

    /// Imports `issue` onto `b`, in the column its state maps to (Backlog
    /// when none). Whether it was imported.
    async fn import(&self, b: &Board, s: &Sync, issue: &Issue) -> Result<bool, DeskError> {
        let state =
            crate::board_sync::column_for(&s.states, &issue.state, issue.state_kind.as_deref())
                .unwrap_or(TicketState::Backlog);
        if state == TicketState::Dropped {
            return Ok(false);
        }
        let at = crate::crew::now();
        let by = Self::pulled_by(s);
        let mut t = Ticket {
            id: Uuid::new_v4(),
            board: b.id,
            number: 0,
            title: issue.title.clone(),
            body: issue.body.clone().unwrap_or_default(),
            state,
            rank: self.end_rank(b.id, state, Uuid::nil()).await?,
            assignee: None,
            needs: vec![],
            links: vec![],
            external: Some(External {
                system: s.system,
                key: issue.key.clone(),
                id: issue.id.clone(),
                url: issue.url.clone(),
                remote_updated_at: issue.updated_at,
                synced: true,
                noted: None,
                link: s.link,
                made_here: false,
                kept: false,
                flagged: issue.flagged,
                seen: Seen {
                    title: issue.title.clone(),
                    body: issue.body.clone().unwrap_or_default(),
                    state: Some(state),
                    needs: vec![],
                },
            }),
            created_by: by.party.clone(),
            pinned: false,
            snoozed_until: None,
            snoozed_at: None,
            state_at: at,
            created_at: at,
            updated_at: at,
            version: 0,
        };
        if !self.0.store.import_ticket(&mut t).await.map_err(internal)? {
            return Ok(false);
        }
        let text = format!(
            "imported {} “{}”",
            issue.key,
            crate::crew::one_line(&issue.title, 120)
        );
        self.ticket_changed(t, &by, Kind::TicketCreated, None, text)
            .await?;
        Ok(true)
    }

    /// Every ticket this link imported whose issue no longer matches the
    /// filter goes to the Left-the-filter shelf; one whose issue is gone, or
    /// moved out of the project, is Dropped. Never deleted, here or there.
    async fn sweep(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        out: &mut Pulled,
    ) -> Result<(), RemoteError> {
        let fail = |e: eyre::Report| RemoteError::Failed(e.to_string());
        let (all, complete) = self.matching(s, remote, None, budget).await?;
        // Only a full answer can say what isn't in it.
        if !complete {
            return Ok(());
        }
        let matching: std::collections::HashSet<String> = all.into_iter().map(|i| i.key).collect();
        let tickets = self.0.store.board_tickets(b.id).await.map_err(fail)?;
        for t in tickets {
            let Some(e) = &t.external else { continue };
            if !e.synced || e.made_here || e.kept || e.link != s.link || matching.contains(&e.key) {
                continue;
            }
            if t.state == TicketState::Dropped {
                continue;
            }
            let tool = match s.system {
                System::Jira => ("jira.get_issue", json!({ "issue_key": e.key })),
                System::Linear => ("linear.issue_get", json!({ "identifier": e.key })),
            };
            budget.take()?;
            let gone = match crate::board_sync::call(remote, s.system, tool.0, tool.1).await {
                Ok(v) => {
                    let read = match s.system {
                        System::Jira => crate::board_sync::jira_issue(&v, ""),
                        System::Linear => {
                            crate::board_sync::linear_issue(v.get("issue").unwrap_or(&v))
                        }
                    };
                    // Moved to another project: no longer this board's.
                    read.is_none_or(|i| !crate::board_sync::key_in_target(&i.key, &s.target))
                }
                Err(RemoteError::Failed(why)) => {
                    let w = why.to_ascii_lowercase();
                    w.contains("404") || w.contains("not found") || w.contains("does not exist")
                }
                Err(other) => return Err(other),
            };
            let changed = match gone {
                true => self.drop_gone(b, s, t).await,
                false => self.detach(b, s, t).await,
            }
            .map_err(|e| RemoteError::Failed(e.to_string()))?;
            match (changed, gone) {
                (true, true) => out.dropped += 1,
                (true, false) => out.detached += 1,
                _ => {}
            }
        }
        Ok(())
    }

    /// `t`'s issue left the filter: to the shelf, unsynced.
    async fn detach(&self, b: &Board, s: &Sync, mut t: Ticket) -> Result<bool, DeskError> {
        let Some(e) = t.external.as_mut() else {
            return Ok(false);
        };
        e.synced = false;
        self.save_ticket(&mut t).await?;
        let key = t.key(b).unwrap_or_default();
        let text = format!(
            "{key} no longer matches the board's filter: it is on the Left-the-filter shelf"
        );
        self.ticket_changed(t, &Self::pulled_by(s), Kind::TicketEdited, None, text)
            .await?;
        Ok(true)
    }

    /// `t`'s issue was deleted or cancelled there: Dropped here.
    async fn drop_gone(&self, b: &Board, s: &Sync, mut t: Ticket) -> Result<bool, DeskError> {
        let from = t.state;
        let was = t.assignee.take();
        self.to_column(&mut t, TicketState::Dropped, crate::crew::now())
            .await?;
        if let Some(e) = t.external.as_mut() {
            e.synced = false;
            e.seen.state = Some(TicketState::Dropped);
        }
        self.save_ticket(&mut t).await?;
        let key = t.key(b).unwrap_or_default();
        if let Some(session) = was.as_ref().and_then(crate::line::session_of) {
            let by = Self::pulled_by(s);
            self.tell_session(
                session,
                format!(
                    "{key} was deleted on {}; it is no longer yours.",
                    s.system.title()
                ),
                &by,
            )
            .await;
        }
        let text = format!(
            "{key} is gone from {}: moved from {} to Dropped",
            s.system.title(),
            from.title()
        );
        self.ticket_changed(t, &Self::pulled_by(s), Kind::TicketMoved, was, text)
            .await?;
        Ok(true)
    }

    /// The person keeps a ticket from the Left-the-filter shelf (it stays
    /// here, unsynced, for good) or drops it (Dropped here; its issue is
    /// never touched).
    pub async fn left_filter(
        &self,
        id: Uuid,
        keep: bool,
        by: &By,
    ) -> Result<crate::service::TicketView, DeskError> {
        let mut t = self.ticket_of(id).await?;
        if !t.left_filter() {
            return Err(DeskError::Conflict(
                "That ticket isn't on the Left-the-filter shelf.".into(),
            ));
        }
        let b = self.board_of(t.board).await?;
        let key = t.key(&b).unwrap_or_default();
        match keep {
            true => {
                if let Some(e) = t.external.as_mut() {
                    e.kept = true;
                }
                self.save_ticket(&mut t).await?;
                let text = format!("kept {key} on the board, no longer synced");
                self.ticket_changed(t, by, Kind::TicketEdited, None, text)
                    .await
            }
            false => {
                let from = t.state;
                let was = t.assignee.take();
                self.to_column(&mut t, TicketState::Dropped, crate::crew::now())
                    .await?;
                self.save_ticket(&mut t).await?;
                let text = format!(
                    "dropped {key}, which left the filter (moved from {})",
                    from.title()
                );
                self.ticket_changed(t, by, Kind::TicketMoved, was, text)
                    .await
            }
        }
    }
}

#[cfg(test)]
#[path = "board_sync_tests.rs"]
mod tests;
