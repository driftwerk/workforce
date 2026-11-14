//! Boards synced with Linear or Jira ([`crate::board_sync`]) as the desk
//! runs them: the hubs a board can sync through, linking and unlinking a
//! board, pausing it. A board's link belongs to its group: it imports only
//! its own project's or team's issues, onto itself, and is paused when it
//! leaves the group it was linked in.

use std::collections::BTreeMap;

use serde::Deserialize;
use uuid::Uuid;

use serde_json::{json, Value};

use super::{Desk, DeskError};
use crate::board::{
    self, Board, External, Filter, Listing, Paused, Retry, Seen, Sync, System, Ticket, TicketState,
    Via,
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
    /// The agent there whose tools it syncs through: the hub's to find.
    #[serde(skip)]
    pub agent: Option<String>,
    pub filter: Filter,
    /// Who the person is there (their Linear user id or Jira accountId),
    /// for "assigned to me", Mine and Take it: the hub's to look up, or
    /// the person's to say where it can't.
    #[serde(default)]
    pub me: Option<String>,
    /// Their name there, as the hub said it.
    #[serde(default)]
    pub me_name: Option<String>,
    /// Where `me` came from: the hub's to say, never the asker's. Without
    /// it, an id given is one the person typed.
    #[serde(skip)]
    pub me_source: Option<board::MeSource>,
    /// Why the hub can't vouch for who the person is.
    #[serde(skip)]
    pub me_note: Option<String>,
    /// The person said the hub's answer of who they are isn't them: it is
    /// not kept, and Take it waits until they check again.
    #[serde(default)]
    pub not_me: bool,
    /// Link a board that has tickets of its own: they stay on Desk only,
    /// with their own keys, and the issues come in beside them.
    #[serde(default)]
    pub keep_tickets: bool,
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
        let own = !self
            .0
            .store
            .board_tickets(id)
            .await
            .map_err(internal)?
            .is_empty();
        if own && !link.keep_tickets {
            return Err(DeskError::Conflict(format!(
                "{} has tickets of its own; link an empty board, or make a new one for {}.",
                b.name, link.target
            )));
        }
        // Its own tickets keep their keys, so the board keeps its prefix.
        let keep_prefix = own && link.keep_tickets;
        let local_before = keep_prefix.then(crate::crew::now);
        let sync = Sync {
            link: Uuid::new_v4(),
            system: link.system,
            target: link.target,
            target_id: link.target_id,
            via: link.via,
            agent: link.agent,
            person: person.to_string(),
            filter: link.filter,
            me_source: link
                .me_source
                .or(link.me.is_some().then_some(board::MeSource::Typed)),
            me: link.me,
            me_name: link.me_name,
            me_note: link.me_note,
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
            listing: None,
            assignees_swept: true,
            local_before,
            fields: None,
            children_next: 0,
        };
        self.0
            .store
            .change_board_sync(id, move |_, prefix| {
                Ok((Some(sync), prefix.filter(|_| keep_prefix)))
            })
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
        // Its issues' files went with the link (`change_board_sync`).
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
pub(super) struct Budget(pub(super) usize);

impl Budget {
    pub(super) fn take(&mut self) -> Result<(), RemoteError> {
        match self.0.checked_sub(1) {
            Some(left) => {
                self.0 = left;
                Ok(())
            }
            None => Err(RemoteError::Spent),
        }
    }
}

/// How far before the latest change a read-through Linear listing saw its
/// cursor goes.
const LINEAR_LOOKBACK: chrono::Duration = chrono::Duration::minutes(2);

/// What a listing of a board's issues read.
struct Listed {
    issues: Vec<Issue>,
    /// Every Linear page gone on from, this listing.
    seen: Vec<String>,
    /// No page was left unread.
    complete: bool,
    /// The Linear page it goes on from, when it isn't complete.
    next: Option<String>,
    /// Read oldest change first (Jira, or a Linear that doesn't page), so
    /// what was read is all there is up to its last.
    ordered: bool,
}

/// How a pull pass ended: the cursor it got to, whether it swept, and the
/// Linear listing it left part read (none when it read it through).
struct PassEnd {
    cursor: Option<String>,
    swept: bool,
    listing: Option<Listing>,
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
                RemoteError::Missing(tool) => Some(Paused::Missing {
                    tools: vec![tool.clone()],
                }),
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
        end: PassEnd,
        retry: BTreeMap<String, Retry>,
    ) -> Result<(), DeskError> {
        let now = crate::crew::now();
        let PassEnd {
            cursor,
            swept,
            listing,
        } = end;
        self.change_sync(id, |s| {
            s.retry = retry;
            s.listing = listing;
            if s.paused.as_ref().is_some_and(retried) {
                s.paused = None;
            }
            s.failures = 0;
            s.backoff_until = None;
            s.problem = None;
            s.pulled_at = Some(now);
            if swept {
                s.swept_at = Some(now);
                s.assignees_swept = true;
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
        let s = self.read_fields(id, s, remote, &mut budget, now).await?;
        // A board linked before who has each issue was kept reads it for
        // every ticket on its first pass.
        let sweep = !s.assignees_swept
            || s.swept_at
                .is_none_or(|at| now - at >= crate::board_sync::SWEEP_EVERY);
        let mut out = Pulled::default();
        let mut retry = s.retry.clone();
        match self
            .pull_pass(&b, &s, remote, &mut budget, sweep, &mut retry, &mut out)
            .await
        {
            Ok(end) => {
                let swept = end.swept;
                self.sync_ok(id, end, retry).await?;
                // Its epics' children, on a sweep, with what the pass left:
                // never the pass's failure. Opening a ticket reads its own.
                if swept {
                    if let Err(e) = self.pull_children(&b, &s, remote, &mut budget).await {
                        tracing::info!(board = %id, error = %e, "the epics' children weren't read")
                    }
                }
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

    /// Keeps on each of board `b`'s Jira epics its children there, whatever
    /// the board's filter: facts on the epic's ticket, never imported. The
    /// epics are searched [`EPICS_PER_SEARCH`](crate::board_sync::EPICS_PER_SEARCH)
    /// at a time, at most [`CHILD_PAGES_PER_PASS`](crate::board_sync::CHILD_PAGES_PER_PASS)
    /// pages a pass, from where the last pass stopped. Each search read to
    /// its end is kept at once; one cut short changes nothing, and is read
    /// first next time. One that failed is passed over. An epic's ticket is
    /// saved and announced only when what it holds changed.
    async fn pull_children(
        &self,
        b: &Board,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
    ) -> Result<(), RemoteError> {
        if s.system != System::Jira {
            return Ok(());
        }
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let mut epics: Vec<(Uuid, String)> = self
            .0
            .store
            .board_tickets(b.id)
            .await
            .map_err(|e| fail(internal(e)))?
            .into_iter()
            .filter_map(|t| {
                let e = t.external.as_ref()?;
                let ours = e.link == s.link && e.synced && !e.kept;
                let key = crate::board_sync::parse_issue_key(&e.key).filter(|k| *k == e.key)?;
                (ours && crate::board_sync::is_epic(&e.details.facts)).then_some((t.id, key))
            })
            .collect();
        if epics.is_empty() {
            return Ok(());
        }
        epics.sort_by(|a, b| a.1.cmp(&b.1));
        let chunks: Vec<&[(Uuid, String)]> =
            epics.chunks(crate::board_sync::EPICS_PER_SEARCH).collect();
        let start = s.children_next % chunks.len();
        let mut pages = Budget(crate::board_sync::CHILD_PAGES_PER_PASS);
        let mut next = start;
        for i in 0..chunks.len() {
            let at = (start + i) % chunks.len();
            let keys: Vec<String> = chunks[at].iter().map(|(_, k)| k.clone()).collect();
            match self
                .read_children(s, remote, budget, &mut pages, &keys)
                .await
            {
                Ok(Some(found)) => {
                    self.keep_epic_children(chunks[at], found)
                        .await
                        .map_err(fail)?;
                    next = at + 1;
                }
                // More pages than a pass may read, from its first search:
                // it never would, so it is passed over.
                Ok(None) if i == 0 => {
                    tracing::info!(board = %b.id, epics = ?keys, "the epics' children are too many to read");
                    next = at + 1;
                }
                // Out of pages or calls: this one goes first next time.
                Ok(None) | Err(RemoteError::Spent) => {
                    next = at;
                    break;
                }
                // Jira refused this search (an epic gone, say): the others
                // are still read.
                Err(RemoteError::Failed(why)) => {
                    tracing::info!(board = %b.id, epics = ?keys, error = %why, "the epics' children weren't read");
                    next = at + 1;
                }
                Err(e) => return Err(e),
            }
        }
        let next = next % chunks.len();
        if next != s.children_next {
            self.change_sync(b.id, |s| s.children_next = next)
                .await
                .map_err(fail)?;
        }
        Ok(())
    }

    /// Keeps on each of `epics` (ticket, key) the children `found` says it
    /// has (none, when it lists none), saving and announcing each epic
    /// whose children changed.
    async fn keep_epic_children(
        &self,
        epics: &[(Uuid, String)],
        mut found: crate::board_sync::Children,
    ) -> Result<(), DeskError> {
        for (id, key) in epics {
            let children = found.of.remove(key).unwrap_or_default();
            let mut t = self.ticket_of(*id).await?;
            let Some(e) = t.external.as_mut() else {
                continue;
            };
            let mut facts = e.details.facts.clone();
            crate::board_sync::set_children(&mut facts, children, &found.blocked, true);
            if facts == e.details.facts {
                continue;
            }
            e.details.facts = facts;
            self.save_ticket(&mut t).await?;
            if let Ok(v) = self.view_ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        Ok(())
    }

    /// The children of the Jira issues `parents` (keys), whatever any
    /// board's filter says, in one search read through within `budget` (a
    /// call a page) and `pages`; `None` when `pages` ran out first.
    pub(super) async fn read_children(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        pages: &mut Budget,
        parents: &[String],
    ) -> Result<Option<crate::board_sync::Children>, RemoteError> {
        let priorities = crate::board_sync::priorities(s);
        let mut found = crate::board_sync::Children::default();
        if !parents
            .iter()
            .any(|k| crate::board_sync::parse_issue_key(k).is_some())
        {
            return Ok(Some(found));
        }
        let mut page: Option<String> = None;
        loop {
            if pages.take().is_err() {
                return Ok(None);
            }
            budget.take()?;
            let args =
                crate::board_sync::children_args(parents, s.fields.as_ref(), page.as_deref());
            let answer = crate::board_sync::call(remote, System::Jira, "jira.search", args).await?;
            let (more, next) = crate::board_sync::jira_children(&answer, priorities);
            found.extend(more);
            match next {
                Some(next) if page.as_deref() == Some(next.as_str()) => {
                    return Err(RemoteError::Failed("Jira gave the same page twice".into()))
                }
                Some(next) => page = Some(next),
                None => return Ok(Some(found)),
            }
        }
    }

    /// `s` with its Jira site's own fields (points, sprint, start date,
    /// Development, priorities) read when they never were, or a day ago.
    /// They are never needed: a hub that can't list them is asked again a
    /// day later, and its cards show none.
    async fn read_fields(
        &self,
        id: Uuid,
        s: Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Sync, DeskError> {
        let fresh = s
            .fields
            .as_ref()
            .is_some_and(|f| now - f.read_at < crate::board_sync::FIELDS_FRESH);
        if s.system != System::Jira || fresh || budget.take().is_err() {
            return Ok(s);
        }
        let fields =
            match crate::board_sync::call_quiet(remote, s.system, "jira.list_fields", json!({}))
                .await
            {
                Ok(answer) => crate::board_sync::jira_fields(&answer, now),
                // What was read before stays: one failed read never takes a
                // board's points and sprints away. Asked again in a day.
                Err(e) => {
                    tracing::info!(board = %id, error = %e, "the Jira site's fields weren't read");
                    board::JiraFields {
                        read_at: now,
                        ..s.fields.clone().unwrap_or_default()
                    }
                }
            };
        let kept = fields.clone();
        Ok(self
            .change_sync(id, move |s| s.fields = Some(kept))
            .await?
            .unwrap_or(Sync {
                fields: Some(fields),
                ..s
            }))
    }

    /// The issues `s`'s filter matches since `since` (all, without), oldest
    /// change first, in the board's project or team only, from the Linear
    /// page `after`; and whether that is all of them (no page was left
    /// unread), and where a Linear listing goes on from when it isn't.
    async fn matching(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        since: Option<&str>,
        after: Option<&str>,
        seen: &[String],
        budget: &mut Budget,
    ) -> Result<Listed, RemoteError> {
        let mut out = Listed {
            issues: vec![],
            seen: seen.to_vec(),
            complete: true,
            next: None,
            ordered: true,
        };
        match s.system {
            System::Jira => {
                // A shared key's "assigned to me" is whom the person's
                // verified email names, never the key's owner.
                let shared_me =
                    s.me.as_deref()
                        .filter(|_| s.me_source.is_some_and(board::MeSource::on_a_shared_key));
                let mut page: Option<String> = None;
                for n in 0.. {
                    if n == crate::board_sync::PAGES_PER_PASS || budget.take().is_err() {
                        out.complete = false;
                        break;
                    }
                    let mut args = json!({
                        "jql": crate::board_sync::jql(&s.target, &s.filter, since, shared_me),
                        "max_results": crate::board_sync::PAGE,
                        "fields": crate::board_sync::JIRA_FIELDS,
                        "custom_fields": crate::board_sync::custom_fields_arg(s.fields.as_ref()),
                    });
                    if let Some(p) = &page {
                        args["next_page_token"] = json!(p);
                    }
                    let answer =
                        crate::board_sync::call(remote, s.system, "jira.search", args).await?;
                    let (found, next) = crate::board_sync::jira_search_issues(&answer, "");
                    out.issues.extend(found);
                    match next {
                        Some(n) => page = Some(n),
                        None => break,
                    }
                }
            }
            System::Linear => {
                let team = s.target_id.as_deref().unwrap_or_default();
                let mut page = after.map(str::to_string);
                for n in 0.. {
                    if n == crate::board_sync::PAGES_PER_PASS || budget.take().is_err() {
                        out.complete = false;
                        out.next = page;
                        break;
                    }
                    let answer = crate::board_sync::call(
                        remote,
                        s.system,
                        "linear.issue_list",
                        crate::board_sync::linear_args(
                            team,
                            &s.filter,
                            s.me.as_deref(),
                            since,
                            page.as_deref(),
                        ),
                    )
                    .await?;
                    let found = crate::board_sync::linear_issues(&answer);
                    // A hub whose tool doesn't page answers one page, which
                    // may not be all.
                    if !crate::board_sync::linear_pages(&answer) {
                        out.complete = found.len() < crate::board_sync::LINEAR_PAGE;
                        out.issues.extend(found);
                        break;
                    }
                    // Linear lists newest first: only a listing read through
                    // moves the cursor.
                    out.ordered = false;
                    out.issues.extend(found);
                    if let Some(p) = &page {
                        if !out.seen.contains(p) {
                            out.seen.push(p.clone());
                        }
                    }
                    match crate::board_sync::linear_next_page(&answer) {
                        // A page named again (itself, or one before it)
                        // would go round for ever.
                        Some(next)
                            if page.as_deref() == Some(next.as_str())
                                || out.seen.contains(&next) =>
                        {
                            return Err(RemoteError::Failed(
                                "Linear gave the same page twice".into(),
                            ))
                        }
                        Some(next) => page = Some(next),
                        None => break,
                    }
                }
                let cursor = since.and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok());
                let done_since = crate::board_sync::done_since(crate::crew::now());
                out.issues.retain(|i| {
                    let label = match &s.filter {
                        Filter::Label(l) => {
                            i.labels.iter().any(|x| x.eq_ignore_ascii_case(l.trim()))
                        }
                        _ => true,
                    };
                    // What a hub that can't filter by state answered: the
                    // whole listing is the open issues, and the recently
                    // done when the board takes them.
                    let open = match (&s.filter, since) {
                        (Filter::All { include_done }, None) => {
                            !i.is_closed(&s.states) || (*include_done && i.updated_at >= done_since)
                        }
                        _ => true,
                    };
                    label && open && cursor.is_none_or(|c| i.updated_at >= c)
                });
            }
        }
        out.issues
            .retain(|i| crate::board_sync::key_in_target(&i.key, &s.target));
        out.issues.sort_by_key(|i| i.updated_at);
        Ok(out)
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
    ) -> Result<PassEnd, RemoteError> {
        let now = crate::crew::now();
        // A pass that stops early keeps where a listing had got to.
        let stopped = |cursor: Option<String>| PassEnd {
            cursor,
            swept: false,
            listing: s.listing.clone(),
        };
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
                Err(RemoteError::Spent) => return Ok(stopped(None)),
                Err(e) => return Err(e),
            }
        }
        // A Linear listing goes on where the last pass left it, while it is
        // still the listing from this cursor.
        let resume = s.listing.as_ref().filter(|l| l.since == s.cursor);
        let started = resume
            .and_then(|l| l.started.clone())
            .unwrap_or_else(|| now.to_rfc3339());
        let listed = match self
            .matching(
                s,
                remote,
                s.cursor.as_deref(),
                resume.map(|l| l.after.as_str()),
                resume.map_or(&[][..], |l| l.seen.as_slice()),
                budget,
            )
            .await
        {
            Ok(listed) => listed,
            Err(e) => {
                // A page gone on from that failed starts over next time.
                if resume.is_some() {
                    let _ = self.change_sync(b.id, |s| s.listing = None).await;
                }
                return Err(e);
            }
        };
        let Listed {
            issues: changed,
            seen,
            complete,
            next,
            ordered,
        } = listed;
        // A listing from no cursor, read through in this pass, is the whole
        // filter: the sweep goes by it rather than read it all again.
        let whole = (s.cursor.is_none() && resume.is_none() && complete).then(|| changed.clone());
        let mut top = resume
            .and_then(|l| l.top.as_deref())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
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
                Err(RemoteError::Spent) => {
                    return Ok(stopped(cursor.filter(|_| ordered)));
                }
                Err(e) => return Err(e),
            }
            top = top.max(Some(issue.updated_at));
            cursor = Some(issue.updated_at.to_rfc3339());
        }
        // Newest first: the cursor moves only once the listing is read
        // through, to the latest change it saw; until then it goes on from
        // its next page.
        if !ordered {
            if !complete {
                return Ok(PassEnd {
                    cursor: None,
                    swept: false,
                    listing: next.map(|after| Listing {
                        after,
                        since: s.cursor.clone(),
                        top: top.map(|t| t.to_rfc3339()),
                        started: Some(started),
                        seen,
                    }),
                });
            }
            // Never past when the listing began, and a little before the
            // latest change it saw: a change made while it was read, or a
            // clock a little off, comes in next time (unchanged ones are
            // skipped without a call).
            let began = chrono::DateTime::parse_from_rfc3339(&started)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc));
            cursor = top
                .map(|t| began.map_or(t, |b| t.min(b)) - LINEAR_LOOKBACK)
                .map(|t| t.to_rfc3339());
        }
        if !complete {
            return Ok(PassEnd {
                cursor,
                swept: false,
                listing: None,
            });
        }
        let end = |swept| PassEnd {
            cursor: cursor.clone(),
            swept,
            listing: None,
        };
        if sweep {
            match self.sweep(b, s, remote, budget, out, whole).await {
                Ok(()) => return Ok(end(true)),
                Err(RemoteError::Spent) => return Ok(end(false)),
                Err(e) => return Err(e),
            }
        }
        Ok(end(false))
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
        let (tool, args) = crate::board_sync::read_issue_call(s.system, key, s.fields.as_ref());
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
            if issue.updated_at <= e.remote_updated_at
                && e.synced
                && !crate::board_sync::has_escaped_newlines(&t.body)
                && !crate::board_sync::rereads_html(e, &t.body)
            {
                return self.refresh_listed(s, t.clone(), issue).await.map_err(fail);
            }
        }
        // A whole board takes in what is open; what is done there comes on
        // only when it takes the recently done, and what isn't on the board
        // yet isn't read for it.
        if existing.is_none() && !Self::comes_on(s, issue) {
            return Ok(());
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
        let (tool, args) =
            crate::board_sync::read_issue_call(System::Jira, &issue.key, s.fields.as_ref());
        let full = crate::board_sync::call(remote, s.system, tool, args).await?;
        let read = crate::board_sync::jira_issue(&full, "")
            .filter(|i| i.key == issue.key)
            .ok_or_else(|| RemoteError::Failed(format!("{} couldn't be read", issue.key)))?;
        Ok(Issue {
            url: issue.url.clone(),
            ..read
        })
    }

    /// Whether `issue`, not on the board, comes onto it: anything the
    /// filter matched, but on a whole board only what is open there, or done
    /// lately when the board takes the recently done.
    fn comes_on(s: &Sync, issue: &Issue) -> bool {
        match &s.filter {
            Filter::All { include_done } if issue.is_closed(&s.states) => {
                *include_done
                    && issue.updated_at >= crate::board_sync::done_since(crate::crew::now())
            }
            _ => true,
        }
    }

    /// Keeps `t` in step with what a listing says of its unchanged issue:
    /// who has it, what it says of itself (its points, sprint, links …) and
    /// what blocks it there. No call is made.
    async fn refresh_listed(
        &self,
        s: &Sync,
        mut t: Ticket,
        issue: &Issue,
    ) -> Result<(), DeskError> {
        let Some(e) = t.external.as_mut() else {
            return Ok(());
        };
        let mut changed = false;
        if let Some(a) = crate::board_sync::assignee_changed_of(e, issue) {
            e.assignee = Some(a);
            changed = true;
        }
        let details = crate::board_sync::details_of(
            issue,
            Some(&e.details),
            crate::board_sync::priorities(s),
        );
        let was = crate::board_sync::blockers(&e.details.facts);
        if details != e.details {
            e.details = details;
            changed = true;
        }
        changed |= self.pulled_needs(s, &mut t, &was, issue).await?;
        if !changed {
            return Ok(());
        }
        self.save_ticket(&mut t).await?;
        if let Ok(v) = self.view_ticket(t).await {
            self.announce(DeskEvent::Ticket {
                ticket: Box::new(v),
            });
        }
        Ok(())
    }

    /// Makes what blocks `t`'s issue there (`issue.blocked_by`) its needs,
    /// where the blocker is a ticket on the same board, and drops a need a
    /// pull made once the issue no longer says so (`was`: what it said).
    /// A need made here stays. Each is seen as sent, so a push never sends
    /// it back. Whether anything changed; `t` is not saved.
    async fn pulled_needs(
        &self,
        s: &Sync,
        t: &mut Ticket,
        was: &[String],
        issue: &Issue,
    ) -> Result<bool, DeskError> {
        let Some(now) = &issue.blocked_by else {
            return Ok(false);
        };
        let mut changed = false;
        // The board's needs, read once a blocker is to be added: a pull never
        // makes a loop of needs, as the person can't.
        let mut edges: Option<Vec<(Uuid, Uuid)>> = None;
        // Every blocker it names, not only new ones: one imported since is
        // made a need once it is on the board.
        let added = now.iter();
        let gone = was.iter().filter(|k| !now.contains(k));
        for (key, add) in added.map(|k| (k, true)).chain(gone.map(|k| (k, false))) {
            let Some(other) = self
                .0
                .store
                .ticket_by_external(s.system, s.link, key)
                .await
                .map_err(internal)?
                .filter(|o| o.board == t.board && o.id != t.id)
            else {
                continue;
            };
            let e = t.external.as_mut().expect("a synced ticket");
            if add {
                // Already a need; or one the person took off here since it
                // was pulled, which stays off.
                if t.needs.contains(&other.id) || e.seen.needs.contains(key) {
                    continue;
                }
                if edges.is_none() {
                    edges = Some(self.0.store.ticket_edges().await.map_err(internal)?);
                }
                let edges = edges.as_mut().expect("read above");
                if board::cycles(t.id, other.id, edges) {
                    tracing::debug!(ticket = %t.id, blocker = %key, "a pulled blocker would make a loop of needs");
                    continue;
                }
                self.0
                    .store
                    .put_need(t.id, other.id)
                    .await
                    .map_err(internal)?;
                edges.push((t.id, other.id));
                t.needs.push(other.id);
                e.seen.needs.push(key.clone());
            } else {
                // No longer blocked there: a need taken off here by hand is
                // forgotten too, so a new block there is pulled again.
                if !t.needs.contains(&other.id) {
                    if e.seen.needs.contains(key) {
                        e.seen.needs.retain(|n| n != key);
                        changed = true;
                    }
                    continue;
                }
                self.0
                    .store
                    .delete_need(t.id, other.id)
                    .await
                    .map_err(internal)?;
                t.needs.retain(|n| *n != other.id);
                e.seen.needs.retain(|n| n != key);
            }
            changed = true;
        }
        Ok(changed)
    }

    /// Keeps who has `t`'s issue in step with `issue`, quietly: no history,
    /// only the ticket announced, so a board catches up without noise.
    async fn refresh_assignee(&self, mut t: Ticket, issue: &Issue) -> Result<(), DeskError> {
        let Some(a) = crate::board_sync::assignee_changed(&t, issue) else {
            return Ok(());
        };
        if let Some(e) = t.external.as_mut() {
            e.assignee = Some(a);
        }
        self.save_ticket(&mut t).await?;
        if let Ok(v) = self.view_ticket(t).await {
            self.announce(DeskEvent::Ticket {
                ticket: Box::new(v),
            });
        }
        Ok(())
    }

    fn pulled_by(s: &Sync) -> By {
        By {
            party: crate::board_sync::remote_party(s.system, &s.target),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
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
        if let Some(a) = &p.assignee {
            said.push(match a {
                Some(a) => format!(
                    "assigned it to {} there",
                    crate::board_sync::defanged_name(&a.name)
                ),
                None => "unassigned it there".to_string(),
            });
        }
        let e = t.external.as_mut().expect("a synced ticket");
        if let Some(a) = p.assignee.clone() {
            e.assignee = Some(a);
        }
        // A body read as ADF stays while the pull reads the HTML it took
        // the place of: the description is the same there.
        let kept_adf = issue
            .body
            .as_deref()
            .is_some_and(|b| crate::board_sync::stands_for(e, b));
        if p.body.is_some() || (issue.body.is_some() && !kept_adf) {
            e.clipped = issue.clipped;
            // Read with today's reader: never read again for its old shape.
            e.html_read = crate::board_sync::HTML_READ;
            e.html_print = None;
        }
        let was = crate::board_sync::blockers(&e.details.facts);
        e.details = crate::board_sync::details_of(
            issue,
            Some(&e.details),
            crate::board_sync::priorities(s),
        );
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
        let needs = self.pulled_needs(s, &mut t, &was, issue).await?;
        // Its files never fail its pull: the list is kept next time.
        let files = match &issue.files {
            Some(files) => match self
                .0
                .store
                .put_remote_files(t.id, files, &Self::pulled_by(s).party)
                .await
            {
                Ok(changed) => changed,
                Err(e) => {
                    tracing::warn!(ticket = %t.id, error = %e, "a ticket's files weren't kept");
                    false
                }
            },
            None => false,
        };
        if said.is_empty() {
            self.save_ticket(&mut t).await?;
            if files || needs {
                if let Ok(v) = self.view_ticket(t).await {
                    self.announce(DeskEvent::Ticket {
                        ticket: Box::new(v),
                    });
                }
            }
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
        let state = issue.column(&s.states).unwrap_or(TicketState::Backlog);
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
                assignee: issue.assignee.clone(),
                clipped: issue.clipped,
                html_read: crate::board_sync::HTML_READ,
                html_print: None,
                details: crate::board_sync::details_of(
                    issue,
                    None,
                    crate::board_sync::priorities(s),
                ),
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
        if let Some(files) = &issue.files {
            if let Err(e) = self.0.store.put_remote_files(t.id, files, &by.party).await {
                tracing::warn!(ticket = %t.id, error = %e, "an imported ticket's files weren't kept");
            }
        }
        // What blocks it there, among the tickets already on the board.
        if self.pulled_needs(s, &mut t, &[], issue).await? {
            self.save_ticket(&mut t).await?;
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
        whole: Option<Vec<Issue>>,
    ) -> Result<(), RemoteError> {
        let fail = |e: eyre::Report| RemoteError::Failed(e.to_string());
        let issues = match whole {
            Some(issues) => issues,
            None => {
                let listed = self.matching(s, remote, None, None, &[], budget).await?;
                // Only a full answer can say what isn't in it.
                if !listed.complete {
                    return Ok(());
                }
                listed.issues
            }
        };
        let all: std::collections::HashMap<String, Issue> =
            issues.into_iter().map(|i| (i.key.clone(), i)).collect();
        let tickets = self.0.store.board_tickets(b.id).await.map_err(fail)?;
        for t in tickets {
            let Some(e) = &t.external else { continue };
            if !e.synced || e.kept || e.link != s.link {
                continue;
            }
            // Who has it is kept in step from the listing itself.
            if let Some(issue) = all.get(&e.key) {
                self.refresh_listed(s, t.clone(), issue)
                    .await
                    .map_err(|e| RemoteError::Failed(e.to_string()))?;
                continue;
            }
            if e.made_here || t.state == TicketState::Dropped {
                continue;
            }
            // A whole board lists what is open: a ticket already Done here
            // is where its issue's state put it.
            if s.filter.is_all() && t.state == TicketState::Done {
                continue;
            }
            let tool = crate::board_sync::read_issue_call(s.system, &e.key, s.fields.as_ref());
            budget.take()?;
            let mut closed = None;
            let gone = match crate::board_sync::call(remote, s.system, tool.0, tool.1).await {
                Ok(v) => {
                    let read = match s.system {
                        System::Jira => crate::board_sync::jira_issue(&v, ""),
                        System::Linear => {
                            crate::board_sync::linear_issue(v.get("issue").unwrap_or(&v))
                        }
                    };
                    // Moved to another project: no longer this board's.
                    let gone = read
                        .as_ref()
                        .is_none_or(|i| !crate::board_sync::key_in_target(&i.key, &s.target));
                    // Done or cancelled there, off a whole board's listing:
                    // it moves to its column, not to the shelf.
                    closed = read.filter(|i| s.filter.is_all() && i.is_closed(&s.states));
                    gone
                }
                Err(RemoteError::Failed(why)) => {
                    let w = why.to_ascii_lowercase();
                    w.contains("404") || w.contains("not found") || w.contains("does not exist")
                }
                Err(other) => return Err(other),
            };
            if let (false, Some(issue)) = (gone, closed) {
                if self
                    .update_pulled(b, s, t, &issue)
                    .await
                    .map_err(|e| RemoteError::Failed(e.to_string()))?
                {
                    out.updated += 1;
                }
                continue;
            }
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

// ── Taking an issue ──────────────────────────────────────────────────────

impl Desk {
    /// Asks the hub afresh who the person is on board `id`'s Jira or Linear
    /// and keeps its answer on the link: vouched when it answered with their
    /// own key, else cleared with why (an id they typed stays theirs).
    /// `hub` names the hub as the board says it.
    pub async fn recheck_me(
        &self,
        id: Uuid,
        remote: &dyn Remote,
        hub: &str,
    ) -> Result<crate::board_sync::Who, RemoteError> {
        let Some((_, s)) = self
            .board_sync(id)
            .await
            .map_err(|e| RemoteError::Failed(e.to_string()))?
        else {
            return Ok(crate::board_sync::Who::Unsaid);
        };
        let who = crate::board_sync::identify(remote, s.system).await?;
        let note = who.why_not(hub, s.system);
        let kept = who.clone();
        self.change_sync(id, move |s| {
            match kept {
                crate::board_sync::Who::Person(me) => {
                    s.me = Some(me.id);
                    s.me_name = Some(me.name);
                    s.me_source = Some(board::MeSource::Vouched);
                }
                crate::board_sync::Who::ByEmail(me, source) => {
                    s.me = Some(me.id);
                    s.me_name = Some(me.name);
                    s.me_source = Some(source);
                }
                // A vouched id the hub no longer stands behind is dropped;
                // one the person typed is theirs to keep.
                _ if s.me_source == Some(board::MeSource::Typed) => {}
                _ => {
                    s.me = None;
                    s.me_name = None;
                    s.me_source = None;
                }
            }
            s.me_note = note;
        })
        .await
        .map_err(|e| RemoteError::Failed(e.to_string()))?;
        Ok(who)
    }

    /// The person takes ticket `id`'s issue: it is assigned to them there,
    /// as themselves, through `remote` (the route the board syncs by).
    ///
    /// Only ever to them: the hub must vouch for who they are, now, with
    /// their own key (a shared key says who owns the key), and agree with
    /// whom the link knows; an id they typed is never assigned to. Only an
    /// issue the answer says no one has is taken; one someone has, or whose
    /// assignee the answer doesn't say, is left. It is read again after,
    /// and a race lost is said. `hub` names the hub as the board says it.
    pub async fn take_ticket(
        &self,
        id: Uuid,
        remote: &dyn Remote,
        hub: &str,
        by: &By,
    ) -> Result<crate::service::TicketView, DeskError> {
        use crate::board_sync::Who;
        let mut t = self.ticket_of(id).await?;
        let b = self.board_of(t.board).await?;
        let Some(s) = b.sync.clone() else {
            return Err(DeskError::Invalid(format!("{} isn't synced.", b.name)));
        };
        let title = s.system.title();
        if Desk::left_its_group(&b) || s.paused.is_some() {
            return Err(DeskError::Conflict(format!(
                "{}'s sync is paused: resume it to take issues on {title}.",
                b.name
            )));
        }
        let Some(e) = t.external.clone().filter(|e| e.writable_by(s.link)) else {
            return Err(DeskError::Conflict(format!(
                "That ticket isn't synced with {title}, so it can't be taken there."
            )));
        };
        let key = e.key.clone();
        let said = |why: RemoteError| match why {
            RemoteError::Waiting { .. } => DeskError::Conflict(format!(
                "{hub} holds the change for your approval: approve it there, and {key} is yours."
            )),
            why => DeskError::Conflict(format!("{title} said: {why}")),
        };
        if s.me_source == Some(board::MeSource::Typed) {
            return Err(DeskError::Conflict(format!(
                "You typed your {title} id, so Desk can't vouch it's you: Take it needs {hub} to say who you are."
            )));
        }
        // Only someone the hub vouched for, on this board, is assigned to:
        // never one first learnt here, so a link the person said isn't them
        // (or one the hub couldn't vouch for) stays as they left it.
        let known = match (s.me_source, s.me.as_deref()) {
            (Some(source), Some(known)) if source.vouched() => known.to_string(),
            _ => {
                tracing::info!(
                    board = %b.id,
                    "Take it waits on who the person is: POST /api/desk/boards/{}/me",
                    b.id
                );
                return Err(DeskError::Conflict(format!(
                    "Desk doesn't know who you are on {title} yet. Use Check who you are on the board, then take it."
                )));
            }
        };
        // Who the person is, asked now, and only compared: Take it changes
        // nothing about who the board knows.
        let who = crate::board_sync::identify(remote, s.system)
            .await
            .map_err(&said)?;
        let me = match who {
            Who::Person(me) | Who::ByEmail(me, _) if me.id == known => me,
            Who::Person(me) | Who::ByEmail(me, _) => {
                return Err(DeskError::Conflict(format!(
                    "{hub} now says you are {} on {title}, not who this board knew: nothing was assigned. Check who you are on the board, then take it.",
                    me.name
                )))
            }
            other => {
                return Err(DeskError::Conflict(
                    other.why_not(hub, s.system).unwrap_or_default(),
                ))
            }
        };
        let mut budget = Budget(3);
        let now = self
            .fetch_issue(&s, remote, &mut budget, &e.key)
            .await
            .map_err(&said)?
            .ok_or_else(|| {
                DeskError::Conflict(format!("{} isn't in {} any more.", e.key, s.target))
            })?;
        match now.assignee.clone() {
            Some(Some(a)) if a.id == me.id => {}
            Some(Some(a)) => {
                self.refresh_assignee(t, &now).await?;
                return Err(DeskError::Conflict(format!(
                    "{} has {} on {title} already.",
                    a.name, e.key
                )));
            }
            Some(None) => {
                let (tool, args) = crate::board_sync::take_call(s.system, &e.key, &me.id);
                crate::board_sync::call(remote, s.system, tool, args)
                    .await
                    .map_err(&said)?;
                // Read again: someone may have taken it in the meantime.
                let after = self
                    .fetch_issue(&s, remote, &mut budget, &e.key)
                    .await
                    .map_err(&said)?;
                match after.as_ref().and_then(|i| i.assignee.clone()) {
                    Some(Some(a)) if a.id == me.id => {}
                    Some(Some(a)) => {
                        if let Some(i) = &after {
                            self.refresh_assignee(t, i).await?;
                        }
                        return Err(DeskError::Conflict(format!(
                            "{} took {} on {title} just before you.",
                            a.name, e.key
                        )));
                    }
                    _ => {
                        return Err(DeskError::Conflict(format!(
                            "{title} didn't say {} is yours after assigning it: look at it there.",
                            e.key
                        )))
                    }
                }
            }
            None => {
                return Err(DeskError::Conflict(format!(
                    "{hub} can't tell who has {} on {title}, so Desk won't assign it.",
                    e.key
                )))
            }
        }
        if let Some(x) = t.external.as_mut() {
            x.assignee = Some(Some(me));
        }
        self.save_ticket(&mut t).await?;
        let text = format!("took {} on {title}: it is assigned to you there", e.key);
        self.ticket_changed(t, by, Kind::TicketEdited, None, text)
            .await
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

/// What a create may have made already.
enum Made {
    Here((String, Option<String>, String)),
    Elsewhere(String),
    Nothing,
}

/// How long a create held for the person's approval waits before it stops.
const HELD_GIVES_UP: chrono::Duration = chrono::Duration::hours(24);

/// How far off a stuck push is put: until the person changes the ticket.
const STUCK: chrono::Duration = chrono::Duration::days(3650);

/// How long after a create was sent, with nothing found by its ref, before
/// it may be sent again.
const CREATE_SETTLES: chrono::Duration = chrono::Duration::minutes(10);

/// A create made an issue outside the board's project: the board pauses,
/// and it is never made again.
fn made_elsewhere(s: &Sync, key: &str) -> RemoteError {
    RemoteError::Refused(format!(
        "a ticket's issue was made as {key}, outside {}; Desk won't make it again. Move the issue into {} or unlink the board.",
        s.target, s.target
    ))
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
                Err(RemoteError::Spent) => break,
                Err(RemoteError::Stuck(why)) => {
                    self.0
                        .store
                        .push_failed(q.ticket, now + STUCK, Some(&why), None)
                        .await
                        .map_err(internal)?;
                }
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
            // Only what the person made here becomes an issue there: never a
            // session's, an agent's or a plan's words.
            None if t.created_by.kind != crate::crew::PartyKind::You => return Ok(()),
            // A ticket the board had before it was linked stays on Desk only.
            None if s.local_before.is_some_and(|at| t.created_at <= at) => return Ok(()),
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
                        a["description"] = json!(crate::ticket_markdown::linear_out(body));
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
                crate::board_sync::pushed(ext, &sent, noted, push.body.is_some());
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
        let now = crate::crew::now();
        // A create sent before, or held for the person's approval, is never
        // sent again while it may have gone through: what it made is looked
        // for by its ref, anywhere.
        let mut found = None;
        if q.creating_at.is_some() || q.waiting.is_some() {
            match self.find_made(s, remote, budget, &reference).await? {
                Made::Here(made) => found = Some(made),
                Made::Elsewhere(key) => return Err(made_elsewhere(s, &key)),
                Made::Nothing => {
                    if let Some(request_id) = &q.waiting {
                        // Denied, expired, or never answered: it stops here,
                        // said on the ticket, until the person changes it.
                        if q.creating_at.is_some_and(|at| now - at >= HELD_GIVES_UP) {
                            return Err(RemoteError::Stuck(format!(
                                "Creating this on {} waited a day for your approval there and wasn't approved. Approve it on the hub, or change the ticket to try again.",
                                s.system.title()
                            )));
                        }
                        return Err(RemoteError::Waiting {
                            request_id: request_id.clone(),
                        });
                    }
                    // A search may not see a new issue at once.
                    if q.creating_at.is_some_and(|at| now - at < CREATE_SETTLES) {
                        return Err(RemoteError::Failed(
                            "waiting to see whether the last create went through".into(),
                        ));
                    }
                }
            }
        }
        let (key, issue_id, url) = match found {
            Some(made) => made,
            None => {
                let issue_type = match s.system {
                    System::Jira => Some(self.issue_type(s, remote, budget).await?),
                    System::Linear => None,
                };
                self.0
                    .store
                    .creating(t.id, now)
                    .await
                    .map_err(|e| fail(internal(e)))?;
                budget.take()?;
                let (tool, args) = match s.system {
                    System::Jira => {
                        let mut labels = vec![reference.clone()];
                        if let Filter::Label(l) = &s.filter {
                            labels.push(l.trim().to_string());
                        }
                        (
                            "jira.create_issue",
                            json!({
                                "project_key": s.target,
                                "summary": t.title,
                                "issue_type": issue_type,
                                "description": t.body,
                                "labels": labels,
                            }),
                        )
                    }
                    System::Linear => {
                        let body = format!(
                            "{}\n\n(Desk ref: {reference})",
                            crate::ticket_markdown::linear_out(t.body.trim())
                        );
                        (
                            "linear.issue_create",
                            json!({
                                "title": t.title,
                                "description": body.trim(),
                                "team_id": s.target_id.clone().unwrap_or_default(),
                            }),
                        )
                    }
                };
                let made = match crate::board_sync::call(remote, s.system, tool, args).await {
                    Ok(made) => made,
                    // The project takes no such type: trying again won't help.
                    Err(RemoteError::Failed(why))
                        if why.to_ascii_lowercase().contains("issue type")
                            || why.to_ascii_lowercase().contains("issuetype") =>
                    {
                        return Err(RemoteError::Refused(format!(
                            "{} won't take a new issue of type {}: {why}",
                            s.target,
                            issue_type.unwrap_or_default()
                        )))
                    }
                    Err(e) => return Err(e),
                };
                let issue = made.get("issue").unwrap_or(&made);
                let said = issue
                    .get("key")
                    .or_else(|| issue.get("identifier"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match said.as_deref().and_then(crate::board_sync::parse_issue_key) {
                    Some(key) if crate::board_sync::key_in_target(&key, &s.target) => (
                        key,
                        issue["id"].as_str().map(str::to_string),
                        issue["url"].as_str().unwrap_or_default().to_string(),
                    ),
                    // It answered with a key, but not one of this project's:
                    // made, so never made again.
                    _ if said.is_some() => {
                        return Err(made_elsewhere(s, said.as_deref().unwrap_or_default()))
                    }
                    _ => {
                        return Err(RemoteError::Failed(format!(
                            "{} gave no key for the new issue",
                            s.system.title()
                        )))
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
            assignee: None,
            clipped: false,
            // Its body is Desk's own: never read again for an old shape.
            html_read: crate::board_sync::HTML_READ,
            html_print: None,
            details: Default::default(),
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
            preference: None,
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

    /// What a create may have made already, found by its Desk ref
    /// anywhere on that Jira or Linear.
    async fn find_made(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        reference: &str,
    ) -> Result<Made, RemoteError> {
        budget.take()?;
        let found = match s.system {
            System::Jira => {
                let answer = crate::board_sync::call(
                    remote,
                    s.system,
                    "jira.search",
                    json!({ "jql": format!("labels = \"{reference}\""), "max_results": 5 }),
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
        if let Some(i) = found
            .iter()
            .find(|i| crate::board_sync::key_in_target(&i.key, &s.target))
        {
            return Ok(Made::Here((i.key.clone(), i.id.clone(), i.url.clone())));
        }
        Ok(match found.into_iter().next() {
            Some(i) => Made::Elsewhere(i.key),
            None => Made::Nothing,
        })
    }

    /// The issue type a new Jira issue takes: Task when the project has it,
    /// else its first that isn't a sub-task or an epic.
    async fn issue_type(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
    ) -> Result<String, RemoteError> {
        budget.take()?;
        let p = crate::board_sync::call(
            remote,
            s.system,
            "jira.get_project",
            json!({ "project_key": s.target }),
        )
        .await?;
        let names: Vec<String> = p["issue_types"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .map(|n| crate::board_sync::unfence(n).0)
            .collect();
        crate::board_sync::pick_issue_type(&names).ok_or_else(|| {
            RemoteError::Refused(format!("{} takes no issue type Desk can make", s.target))
        })
    }
}

#[cfg(test)]
#[path = "board_sync_tests.rs"]
mod tests;
