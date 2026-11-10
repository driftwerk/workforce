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
    self, Board, External, Filter, Paused, Seen, Sync, System, Ticket, TicketState, Via,
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
            .board_linked_to(link.system, &link.target, id)
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
            None => Err(RemoteError::RateLimited {
                retry_after_secs: None,
            }),
        }
    }
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
    ) -> Result<(), DeskError> {
        let now = crate::crew::now();
        self.change_sync(id, |s| {
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
        match self
            .pull_pass(&b, &s, remote, &mut budget, sweep, &mut out)
            .await
        {
            Ok(cursor) => {
                self.sync_ok(id, cursor, sweep).await?;
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
                    budget.take()?;
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
                budget.take()?;
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
        out: &mut Pulled,
    ) -> Result<Option<String>, RemoteError> {
        let (changed, complete) = self
            .matching(s, remote, s.cursor.as_deref(), budget)
            .await?;
        let mut cursor = None;
        for issue in changed {
            match self.take_in(b, s, remote, budget, &issue, out).await {
                Ok(()) => {}
                // A tool that failed on this one issue: the rest go on, and
                // the cursor stops before it so it is tried again.
                Err(RemoteError::Failed(why)) => {
                    tracing::info!(board = %b.id, issue = %issue.key, error = %why, "an issue wasn't pulled");
                    return Ok(cursor);
                }
                Err(e) => return Err(e),
            }
            cursor = Some(issue.updated_at.to_rfc3339());
        }
        if !complete {
            return Ok(cursor);
        }
        if sweep {
            self.sweep(b, s, remote, budget, out).await?;
        }
        Ok(cursor)
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
            .ticket_by_external(s.system, &issue.key)
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

// ── The push ─────────────────────────────────────────────────────────────

/// What one pass of a board's push did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pushed {
    pub created: usize,
    pub updated: usize,
    pub commented: usize,
    pub waiting: usize,
    pub skipped: Option<String>,
}

/// How long a push the hub holds for the person's approval waits before
/// it is sent again.
const WAITING_RETRY: chrono::Duration = chrono::Duration::hours(1);

impl Desk {
    /// Whether board `id` has no change due to go out now.
    pub async fn nothing_to_push(&self, id: Uuid) -> Result<bool, DeskError> {
        Ok(self
            .0
            .store
            .due_pushes(id, crate::crew::now(), 1)
            .await
            .map_err(internal)?
            .is_empty())
    }

    /// Sends board `id`'s queued changes to their issues through `remote`:
    /// creates what was made here, then each change since its issue was last
    /// seen, and one comment per move into Doing or Review. Only issues this
    /// link imported or made are written to; nothing is ever deleted or
    /// reassigned there.
    pub async fn push_board(&self, id: Uuid, remote: &dyn Remote) -> Result<Pushed, DeskError> {
        let now = crate::crew::now();
        let Some((b, s)) = self.board_sync(id).await? else {
            return Ok(Pushed {
                skipped: Some("not synced".into()),
                ..Default::default()
            });
        };
        if let Some(why) = held(&b, &s, now) {
            return Ok(Pushed {
                skipped: Some(why),
                ..Default::default()
            });
        }
        let due = self
            .0
            .store
            .due_pushes(id, now, 20)
            .await
            .map_err(internal)?;
        let mut budget = Budget(crate::board_sync::CALLS_PER_PASS);
        let mut out = Pushed::default();
        for q in due {
            match self
                .push_one(&b, &s, remote, &mut budget, &q, &mut out)
                .await
            {
                Ok(()) => self
                    .0
                    .store
                    .pushed(q.ticket, q.queued_at)
                    .await
                    .map_err(internal)?,
                Err(RemoteError::Waiting { request_id }) => {
                    out.waiting += 1;
                    self.0
                        .store
                        .push_failed(q.ticket, now + WAITING_RETRY, None, Some(&request_id))
                        .await
                        .map_err(internal)?;
                }
                Err(RemoteError::Failed(why)) => {
                    let next = now + crate::board_sync::backoff(q.tries.max(0) as u32);
                    self.0
                        .store
                        .push_failed(q.ticket, next, Some(&why), None)
                        .await
                        .map_err(internal)?;
                }
                // Budget spent: the rest wait for the next pass.
                Err(RemoteError::RateLimited {
                    retry_after_secs: None,
                }) => break,
                Err(e) => {
                    self.sync_failed(id, &e).await?;
                    out.skipped = Some(e.to_string());
                    break;
                }
            }
        }
        Ok(out)
    }

    /// One queued ticket's push.
    async fn push_one(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        q: &crate::board_sync_store::Queued,
        out: &mut Pushed,
    ) -> Result<(), RemoteError> {
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let t = match self
            .0
            .store
            .ticket(q.ticket)
            .await
            .map_err(|e| fail(internal(e)))?
        {
            Some(t) if t.board == b.id => t,
            // Gone, or on another board: nothing of it goes out from here.
            _ => return Ok(()),
        };
        let mut t = match &t.external {
            None => {
                let made = self.create_issue(b, s, remote, budget, t, q).await?;
                out.created += 1;
                made
            }
            Some(e) if e.writable_by(s.link) => t,
            // Kept, on the shelf, or another link's: never written to.
            Some(_) => return Ok(()),
        };
        let needs = self.need_keys(&t).await.map_err(fail)?;
        let need_keys: Vec<String> = needs.iter().map(|(k, _)| k.clone()).collect();
        let push = crate::board_sync::push_of(&t, &need_keys);
        if push.is_empty() {
            return Ok(());
        }
        let e = t.external.clone().expect("a synced ticket");
        if push.title.is_some() || push.body.is_some() {
            budget.take()?;
            let args = match s.system {
                System::Jira => {
                    let mut a = json!({ "issue_key": e.key });
                    if let Some(title) = &push.title {
                        a["summary"] = json!(title);
                    }
                    if let Some(body) = &push.body {
                        a["description"] = json!(body);
                    }
                    a
                }
                System::Linear => {
                    let mut a = json!({ "identifier": e.key });
                    if let Some(title) = &push.title {
                        a["title"] = json!(title);
                    }
                    if let Some(body) = &push.body {
                        a["description"] = json!(body);
                    }
                    a
                }
            };
            let tool = match s.system {
                System::Jira => "jira.update_issue",
                System::Linear => "linear.issue_update",
            };
            crate::board_sync::call(remote, s.system, tool, args).await?;
        }
        let mut commented = false;
        if let Some(state) = push.state {
            if let Some(name) = s.states.get(&state) {
                self.transition(s, remote, budget, &e.key, name).await?;
            }
            let pr = t.links.iter().rev().find_map(|l| match l {
                crate::board::Link::Pr { url, .. } => Some(url.clone()),
                _ => None,
            });
            if let Some(note) =
                crate::board_sync::note_for(&t, state, t.assignee.as_ref(), pr.as_deref())
            {
                budget.take()?;
                let (tool, args) = match s.system {
                    System::Jira => (
                        "jira.add_comment",
                        json!({ "issue_key": e.key, "body": note }),
                    ),
                    System::Linear => (
                        "linear.comment_add",
                        json!({ "identifier": e.key, "body": note }),
                    ),
                };
                crate::board_sync::call(remote, s.system, tool, args).await?;
                commented = true;
                out.commented += 1;
            }
        }
        // Jira's tools here link no issues: its needs stay Desk's own.
        if s.system == System::Linear {
            for need in push
                .needs
                .iter()
                .filter(|k| crate::board_sync::key_in_target(k, &s.target))
            {
                budget.take()?;
                crate::board_sync::call(
                    remote,
                    s.system,
                    "linear.issue_link",
                    json!({ "identifier": e.key, "related_identifier": need, "type": "blocked_by" }),
                )
                .await?;
            }
        }
        // What the issue now says, as far as this push knows: kept on the
        // ticket as it is now, so what changed meanwhile goes next time.
        let sent = Seen {
            title: t.title.clone(),
            body: t.body.clone(),
            state: Some(t.state),
            needs: need_keys,
        };
        let noted = crate::board_sync::noted_after(e.noted, t.state, commented);
        for _ in 0..3 {
            if let Some(ext) = t.external.as_mut() {
                ext.seen = sent.clone();
                ext.noted = noted;
            }
            match self.0.store.put_ticket(&mut t).await {
                Ok(()) => {
                    out.updated += 1;
                    if crate::board_sync::push_of(&t, &sent.needs) != Default::default() {
                        let _ = self
                            .0
                            .store
                            .queue_push(t.id, t.board, crate::crew::now())
                            .await;
                    }
                    return Ok(());
                }
                Err(crate::board_store::TicketWrite::Stale) => {
                    t = self.ticket_of(t.id).await.map_err(fail)?;
                }
                Err(e) => return Err(RemoteError::Failed(e.to_string())),
            }
        }
        Err(RemoteError::Failed(
            "the ticket kept changing while it was pushed".into(),
        ))
    }

    /// Moves the issue `key` to the remote state `name`.
    async fn transition(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        key: &str,
        name: &str,
    ) -> Result<(), RemoteError> {
        budget.take()?;
        match s.system {
            System::Jira => {
                let list = crate::board_sync::call(
                    remote,
                    s.system,
                    "jira.list_transitions",
                    json!({ "issue_key": key }),
                )
                .await?;
                let Some(id) = list["transitions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|t| {
                        t["to_status"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .and_then(|t| t["id"].as_str().map(str::to_string))
                else {
                    // No way there from where it is: the state stays Desk's.
                    return Ok(());
                };
                budget.take()?;
                crate::board_sync::call(
                    remote,
                    s.system,
                    "jira.transition_issue",
                    json!({ "issue_key": key, "transition": id }),
                )
                .await
                .map(drop)
            }
            System::Linear => {
                let list = crate::board_sync::call(
                    remote,
                    s.system,
                    "linear.states_list",
                    json!({ "team_id": s.target_id.clone().unwrap_or_default() }),
                )
                .await?;
                let Some(id) = list["states"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|st| {
                        st["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .and_then(|st| st["id"].as_str().map(str::to_string))
                else {
                    return Ok(());
                };
                budget.take()?;
                crate::board_sync::call(
                    remote,
                    s.system,
                    "linear.issue_update",
                    json!({ "identifier": key, "state_id": id }),
                )
                .await
                .map(drop)
            }
        }
    }

    /// Creates the issue for `t`, made here on a synced board, and gives `t`
    /// its key. A create that may have gone through before is looked for by
    /// its Desk ref first, so it is never made twice.
    async fn create_issue(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        mut t: Ticket,
        q: &crate::board_sync_store::Queued,
    ) -> Result<Ticket, RemoteError> {
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let reference = crate::board_sync::desk_ref(t.id);
        let found = match q.creating_at {
            Some(_) => self.find_made(s, remote, budget, &reference).await?,
            None => None,
        };
        let (key, issue_id, url) = match found {
            Some(made) => made,
            None => {
                self.0
                    .store
                    .creating(t.id, crate::crew::now())
                    .await
                    .map_err(|e| fail(internal(e)))?;
                budget.take()?;
                match s.system {
                    System::Jira => {
                        let mut labels = vec![reference.clone()];
                        if let Filter::Label(l) = &s.filter {
                            labels.push(l.trim().to_string());
                        }
                        let made = crate::board_sync::call(
                            remote,
                            s.system,
                            "jira.create_issue",
                            json!({
                                "project_key": s.target,
                                "summary": t.title,
                                "issue_type": "Task",
                                "description": t.body,
                                "labels": labels,
                            }),
                        )
                        .await?;
                        let key = made["key"]
                            .as_str()
                            .and_then(crate::board_sync::parse_issue_key)
                            .filter(|k| crate::board_sync::key_in_target(k, &s.target))
                            .ok_or_else(|| {
                                RemoteError::Failed("Jira gave no key for the new issue".into())
                            })?;
                        (key, made["id"].as_str().map(str::to_string), String::new())
                    }
                    System::Linear => {
                        let body = format!("{}\n\n(Desk ref: {reference})", t.body.trim());
                        let made = crate::board_sync::call(
                            remote,
                            s.system,
                            "linear.issue_create",
                            json!({
                                "title": t.title,
                                "description": body.trim(),
                                "team_id": s.target_id.clone().unwrap_or_default(),
                            }),
                        )
                        .await?;
                        let issue = made.get("issue").unwrap_or(&made);
                        let key = issue["identifier"]
                            .as_str()
                            .and_then(crate::board_sync::parse_issue_key)
                            .filter(|k| crate::board_sync::key_in_target(k, &s.target))
                            .ok_or_else(|| {
                                RemoteError::Failed("Linear gave no key for the new issue".into())
                            })?;
                        (
                            key,
                            issue["id"].as_str().map(str::to_string),
                            issue["url"].as_str().unwrap_or_default().to_string(),
                        )
                    }
                }
            }
        };
        let at = crate::crew::now();
        t.external = Some(External {
            system: s.system,
            key: key.clone(),
            id: issue_id,
            url,
            remote_updated_at: at,
            synced: true,
            noted: None,
            link: s.link,
            made_here: true,
            kept: false,
            flagged: false,
            // What was sent; its state is the remote's default, so the
            // column's state goes next.
            seen: Seen {
                title: t.title.clone(),
                body: t.body.clone(),
                state: None,
                needs: vec![],
            },
        });
        loop {
            match self.0.store.put_ticket(&mut t).await {
                Ok(()) => break,
                Err(crate::board_store::TicketWrite::Stale) => {
                    let ext = t.external.clone();
                    t = self.ticket_of(t.id).await.map_err(fail)?;
                    t.external = ext;
                }
                Err(e) => return Err(RemoteError::Failed(e.to_string())),
            }
        }
        let by = By {
            party: crate::crew::Party::desk(),
            reason: None,
            suggestion_id: None,
            hop: 0,
        };
        let text = format!("created {key} on {} for it", s.system.title());
        let group = b.group.clone();
        self.note(
            by.event(Kind::TicketEdited, None, text)
                .about_ticket(t.id, group.as_deref()),
        );
        if let Ok(v) = self.view_ticket(t.clone()).await {
            self.announce(crate::model::DeskEvent::Ticket {
                ticket: Box::new(v),
            });
        }
        Ok(t)
    }

    /// An issue a create may have made already, found by its Desk ref.
    async fn find_made(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        reference: &str,
    ) -> Result<Option<(String, Option<String>, String)>, RemoteError> {
        budget.take()?;
        let found = match s.system {
            System::Jira => {
                let answer = crate::board_sync::call(
                    remote,
                    s.system,
                    "jira.search",
                    json!({ "jql": format!("project = \"{}\" AND labels = \"{reference}\"", s.target), "max_results": 2 }),
                )
                .await?;
                crate::board_sync::jira_search_issues(&answer, "").0
            }
            System::Linear => {
                let answer = crate::board_sync::call(
                    remote,
                    s.system,
                    "linear.issue_search",
                    json!({ "query": reference, "limit": 5 }),
                )
                .await?;
                crate::board_sync::linear_issues(&answer)
                    .into_iter()
                    .filter(|i| i.body.as_deref().is_some_and(|b| b.contains(reference)))
                    .collect()
            }
        };
        Ok(found
            .into_iter()
            .find(|i| crate::board_sync::key_in_target(&i.key, &s.target))
            .map(|i| (i.key, i.id, i.url)))
    }
}
