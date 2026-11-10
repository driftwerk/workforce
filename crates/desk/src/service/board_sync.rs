//! Boards synced with Linear or Jira ([`crate::board_sync`]) as the desk
//! runs them: the hubs a board can sync through, linking and unlinking a
//! board, pausing it. A board's link belongs to its group: it imports only
//! its own project's or team's issues, onto itself, and is paused when it
//! leaves the group it was linked in.

use std::collections::BTreeMap;

use serde::Deserialize;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::board::{self, Board, Filter, Paused, Sync, System, TicketState, Via};
use crate::board_sync::{check_filter, check_target};
use crate::board_sync_store::SyncHub;
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
