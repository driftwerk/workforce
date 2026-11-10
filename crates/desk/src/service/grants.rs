//! Grants on Desk's side (remote bosses, RB1): this engine's copy of what
//! the home said of each bossed group, the engine-wide pause, and, on a
//! machine, its sidebar made from its grants ([`crate::grant`]).
//!
//! Only the routes in `grant_api` write here: each method that does takes a
//! [`Native`], which nothing outside this crate can make, so no agent tool,
//! crew call or session reaches them. A stop or the pause is in force in
//! memory before it is written; anything that would widen is written
//! first.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::grant::{
    self, Change, Envelope, Floor, Grant, GrantPut, NotLive, Refusal, Stopped, StoppedFrom,
};
use crate::grant_store::{CallRow, Projection};
use crate::model::DeskEvent;

/// Proof that a call comes from the native-only routes. Only this crate
/// makes one.
pub struct Native(());

impl Native {
    pub(crate) fn routes() -> Self {
        Self(())
    }
}

/// The engine-wide pause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Paused {
    pub at: DateTime<Utc>,
    pub from: String,
}

/// The grants as this engine holds them.
#[derive(Debug, Default)]
pub(crate) struct GrantsRun {
    pub(super) grants: BTreeMap<String, Grant>,
    /// What deleted grants, and stops of groups with no grant, left.
    pub(super) floors: BTreeMap<String, Floor>,
    /// Stops in force before they are written (they are, under the write
    /// lock, at once).
    pub(super) stopping: BTreeMap<String, Stopped>,
    paused: Option<Paused>,
    /// When this machine began taking its sessions' groups from grants.
    since: Option<DateTime<Utc>>,
    /// Whether the kept grants were read. Until they are, nothing is
    /// written over them, and a machine's sidebar stays closed.
    loaded: bool,
}

/// A grant as the routes show it: whether it serves its boss now, and why
/// not.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantView {
    #[serde(flatten)]
    pub grant: Grant,
    pub live: bool,
    pub why: Option<String>,
}

/// Every grant here, and the engine's switches.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantsView {
    pub paused: Option<Paused>,
    /// Whether this machine's sessions' groups come from its grants.
    pub stamps_from_grants: bool,
    pub grants: Vec<GrantView>,
    /// Groups whose grant was deleted, or that were stopped with none:
    /// the revision a new grant must pass, and the stop it keeps.
    pub left: BTreeMap<String, Floor>,
}

/// What a write of a grant did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantWritten {
    pub grant: GrantView,
    pub change: Change,
    /// How many sessions' rows were restamped to their grant's group.
    pub restamped: usize,
}

fn hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

fn refused(r: Refusal) -> DeskError {
    match r {
        Refusal::Stale { have } => DeskError::Conflict(format!(
            "This engine has revision {have} of that grant: an older one is never taken."
        )),
        Refusal::Conflict(why) => DeskError::Conflict(why),
        Refusal::Invalid(why) => DeskError::Invalid(why),
    }
}

/// What the audit keeps of a write: what it says, never whom it names.
fn summary(p: &GrantPut) -> String {
    serde_json::json!({
        "grantVersion": p.grant_version,
        "revision": p.revision,
        "boss": p.boss,
        "level": p.level,
        "sees": p.sees,
        "recall": p.recall,
        "members": p.members.len(),
        "leaving": p.leaving.len(),
        "folders": p.folders.len(),
        "repos": p.repos.len(),
        "hours": p.hours,
        "resume": p.resume,
    })
    .to_string()
}

impl Desk {
    /// Reads the grants and the pause kept across restarts. One that can't
    /// be read pauses every boss: the person clears it in Desk.
    pub(super) async fn grants_load(&self) {
        if let Err(e) = self.grants_read().await {
            tracing::error!(
                error = format!("{e:#}"),
                "the grants couldn't be read: every boss here is paused until Desk clears it"
            );
            self.0.grants.lock().paused.get_or_insert_with(|| Paused {
                at: Utc::now(),
                from: "engine".into(),
            });
        }
    }

    async fn grants_read(&self) -> eyre::Result<()> {
        let grants = self.0.store.boss_grants().await?;
        let floors = self.0.store.boss_grant_floors().await?;
        let engine = self.0.store.boss_engine().await?;
        let mut run = self.0.grants.lock();
        run.grants = grants.into_iter().map(|g| (g.group.clone(), g)).collect();
        run.floors = floors.into_iter().collect();
        if engine.paused {
            run.paused.get_or_insert_with(|| Paused {
                at: engine.paused_at.unwrap_or_else(Utc::now),
                from: engine.paused_from.unwrap_or_else(|| "desk".into()),
            });
        }
        run.since = engine.grants_since;
        run.loaded = true;
        Ok(())
    }

    /// Before a write over the kept grants: they were read, or are read now.
    async fn grants_loaded(&self) -> Result<(), DeskError> {
        if self.0.grants.lock().loaded {
            return Ok(());
        }
        self.grants_read().await.map_err(|e| {
            tracing::error!(error = format!("{e:#}"), "the grants still can't be read");
            DeskError::Conflict(
                "This engine can't read the grants it keeps, so it takes no new one: its bosses \
                 stay paused."
                    .into(),
            )
        })
    }

    /// Whether this machine's sessions' groups come from its grants: on a
    /// machine (as its helper said) once it was first granted any.
    pub fn stamps_from_grants(&self) -> bool {
        let run = self.0.grants.lock();
        // One whose grants couldn't be read is taken to have some.
        self.0.opts.bosses.machine && (run.since.is_some() || !run.loaded)
    }

    /// Why the sidebar can't be changed here, when it can't: on a machine
    /// whose groups come from its grants.
    pub fn layout_refused(&self) -> Option<DeskError> {
        self.stamps_from_grants().then(|| {
            DeskError::Conflict(
                "This machine's groups come from Desk on the person's Mac: change them there."
                    .into(),
            )
        })
    }

    /// Whether every boss on this engine is paused.
    pub fn bosses_paused(&self) -> bool {
        self.0.grants.lock().paused.is_some()
    }

    /// `group`'s grant while it serves its boss, or why it doesn't, in
    /// plain words. What every boss's call here is decided under.
    pub fn grant_live(&self, group: &str) -> Result<Grant, String> {
        let run = self.0.grants.lock();
        let Some(g) = run.grants.get(group) else {
            return Err(crate::scope::not_ours(None));
        };
        let mut g = g.clone();
        if let Some(s) = run.stopping.get(group) {
            g.stopped.get_or_insert(*s);
        }
        grant::live(&g, Utc::now(), run.paused.is_some()).map_err(|why| why.say(&g.name))?;
        Ok(g)
    }

    fn grant_view_of(run: &GrantsRun, g: &Grant, now: DateTime<Utc>) -> GrantView {
        let mut g = g.clone();
        if let Some(s) = run.stopping.get(&g.group) {
            g.stopped.get_or_insert(*s);
        }
        let why = grant::live(&g, now, run.paused.is_some())
            .err()
            .map(|w: NotLive| w.say(&g.name));
        GrantView {
            live: why.is_none(),
            why,
            grant: g,
        }
    }

    /// Every grant here, and the engine's switches.
    pub fn grants_view(&self, _: &Native) -> GrantsView {
        let stamps = self.stamps_from_grants();
        let run = self.0.grants.lock();
        let now = Utc::now();
        GrantsView {
            left: run.floors.clone(),
            paused: run.paused.clone(),
            stamps_from_grants: stamps,
            grants: run
                .grants
                .values()
                .map(|g| Self::grant_view_of(&run, g, now))
                .collect(),
        }
    }

    /// `group`'s grant.
    pub fn grant_view(&self, _: &Native, group: &str) -> Option<GrantView> {
        let run = self.0.grants.lock();
        run.grants
            .get(group)
            .map(|g| Self::grant_view_of(&run, g, Utc::now()))
    }

    async fn audit_refused(&self, call: CallRow, why: &DeskError) {
        let call = CallRow {
            outcome: "refused",
            said: Some(why.to_string()),
            ..call
        };
        if let Err(e) = self.0.store.boss_call(&call).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "a refused grant write wasn't audited"
            );
        }
    }

    /// The home's write of `group`'s grant (`body` as sent).
    pub async fn put_grant(
        &self,
        _: Native,
        group: &str,
        body: serde_json::Value,
    ) -> Result<GrantWritten, DeskError> {
        let _one = self.0.grant_write.lock().await;
        self.grants_loaded().await?;
        let mut call = CallRow {
            caller: "desk".into(),
            boss: None,
            group: Some(group.into()),
            tool: "grant.put",
            input: String::new(),
            input_hash: hash(body.to_string().as_bytes()),
            outcome: "ok",
            said: None,
            result: serde_json::json!({}),
        };
        let envelope: Envelope = match serde_json::from_value(body.clone()) {
            Ok(e) => e,
            Err(e) => {
                let why = DeskError::Invalid(format!("that isn't a grant: {e}"));
                self.audit_refused(call, &why).await;
                return Err(why);
            }
        };
        if envelope.grant_version != grant::GRANT_VERSION {
            // Its old copy stays the group's members, and serves nothing.
            let old = self.0.grants.lock().grants.get(group).cloned();
            let why = DeskError::Invalid(grant::unknown_version(envelope.grant_version));
            call.input = serde_json::json!({"grantVersion": envelope.grant_version}).to_string();
            if let Some(u) =
                grant::unreadable(old.as_ref(), envelope.grant_version, envelope.revision)
            {
                call.outcome = "refused";
                call.said = Some(why.to_string());
                self.commit_grants(vec![u], None, call).await?;
            } else {
                self.audit_refused(call, &why).await;
            }
            return Err(why);
        }
        let put: GrantPut = match serde_json::from_value(body) {
            Ok(p) => p,
            Err(e) => {
                let why = DeskError::Invalid(format!("that isn't a grant: {e}"));
                self.audit_refused(call, &why).await;
                return Err(why);
            }
        };
        call.input = summary(&put);
        call.boss = put.boss.clone();
        let now = Utc::now();
        let accepted = {
            let run = self.0.grants.lock();
            let others: Vec<&Grant> = run.grants.values().collect();
            grant::accept(
                run.grants.get(group),
                run.floors.get(group),
                &put,
                group,
                &others,
                now,
                self.0.opts.bosses.grant_hours,
            )
        };
        let accepted = match accepted {
            Ok(a) => a,
            Err(r) => {
                let why = refused(r);
                self.audit_refused(call, &why).await;
                return Err(why);
            }
        };
        let mut writes = vec![accepted.grant.clone()];
        {
            let run = self.0.grants.lock();
            let mut from: HashMap<&str, Grant> = HashMap::new();
            for (other, s) in &accepted.taken {
                if let Some(o) = run.grants.get(other) {
                    from.entry(other.as_str())
                        .or_insert_with(|| o.clone())
                        .born
                        .remove(s);
                }
            }
            writes.extend(from.into_values());
        }
        call.result = serde_json::json!({"change": accepted.change});
        let restamped = self.commit_grants(writes, None, call).await?;
        let grant = self
            .grant_view(&Native::routes(), group)
            .ok_or(DeskError::NotFound)?;
        Ok(GrantWritten {
            grant,
            change: accepted.change,
            restamped,
        })
    }

    /// The home took `group`'s grant away: its sessions are in no group
    /// here (on a machine), and it serves no one.
    pub async fn delete_grant(&self, _: Native, group: &str) -> Result<(), DeskError> {
        let _one = self.0.grant_write.lock().await;
        self.grants_loaded().await?;
        let call = CallRow {
            caller: "desk".into(),
            boss: None,
            group: Some(group.into()),
            tool: "grant.delete",
            input: "{}".into(),
            input_hash: hash(group.as_bytes()),
            outcome: "ok",
            said: None,
            result: serde_json::json!({}),
        };
        if !self.0.grants.lock().grants.contains_key(group) {
            let why = DeskError::NotFound;
            self.audit_refused(call, &why).await;
            return Err(why);
        }
        // A stop in force, written or not, stays with what is left.
        let stopped = {
            let run = self.0.grants.lock();
            run.grants
                .get(group)
                .and_then(|g| g.stopped)
                .or_else(|| run.stopping.get(group).copied())
        };
        self.commit_grants(Vec::new(), Some((group, stopped)), call)
            .await?;
        self.0.grants.lock().stopping.remove(group);
        Ok(())
    }

    /// Stops `group`'s boss, or with none every boss here and pauses the
    /// engine: in force at once, then written. Nothing the home sends later
    /// undoes it; only Desk resumes a grant or clears the pause. A group
    /// with no grant keeps the stop for when it gets one. Where the grants
    /// can't be read, the stop is kept as the engine's pause.
    pub async fn stop_bosses(
        &self,
        _: Native,
        group: Option<&str>,
        from: StoppedFrom,
    ) -> Result<GrantsView, DeskError> {
        if group.is_some_and(|g| !grant::group_id_ok(g)) {
            return Err(DeskError::Invalid("that can't be a group's id".into()));
        }
        let now = Utc::now();
        let stop = Stopped { at: now, from };
        let paused = Paused {
            at: now,
            from: from.as_str().into(),
        };
        {
            let mut run = self.0.grants.lock();
            if group.is_none() {
                run.paused.get_or_insert_with(|| paused.clone());
            }
            let groups: Vec<String> = match group {
                Some(g) => vec![g.to_string()],
                None => run.grants.keys().cloned().collect(),
            };
            for g in groups {
                run.stopping.entry(g).or_insert(stop);
            }
        }
        let _one = self.0.grant_write.lock().await;
        // Grants that couldn't be read at the start are read now if they
        // can be, so a stop finds them; if not, the pause holds it.
        let loaded = self.grants_loaded().await.is_ok();
        let call = CallRow {
            caller: from.as_str().into(),
            boss: None,
            group: group.map(str::to_string),
            tool: "bosses.stop",
            input: serde_json::json!({"group": group}).to_string(),
            input_hash: hash(group.unwrap_or("").as_bytes()),
            outcome: "ok",
            said: None,
            result: serde_json::json!({"loaded": loaded}),
        };
        let pausing = group.is_none() || !loaded;
        if pausing {
            self.0
                .grants
                .lock()
                .paused
                .get_or_insert_with(|| paused.clone());
            self.0
                .store
                .put_bosses_paused(true, Some(from.as_str()), &call)
                .await?;
            // A clear that ran meanwhile can't leave it let go here while it
            // is kept paused.
            self.0.grants.lock().paused.get_or_insert(paused);
        }
        let stops: Vec<(String, Stopped)> = {
            let run = self.0.grants.lock();
            match group {
                Some(g) => vec![(g.to_string(), run.stopping.get(g).copied().unwrap_or(stop))],
                None => run
                    .grants
                    .keys()
                    .map(|g| (g.clone(), run.stopping.get(g).copied().unwrap_or(stop)))
                    .collect(),
            }
        };
        let kept = self
            .0
            .store
            .stop_grants(&stops, (!pausing).then_some(&call))
            .await;
        if let Err(e) = kept {
            // The pause, kept, holds what couldn't be written.
            if pausing {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "a stop is kept as the engine's pause"
                );
                return Ok(self.grants_view(&Native::routes()));
            }
            return Err(e.into());
        }
        {
            let mut run = self.0.grants.lock();
            for (g, s) in &stops {
                match run.grants.get_mut(g) {
                    Some(x) => {
                        x.stopped.get_or_insert(*s);
                    }
                    None => {
                        run.floors
                            .entry(g.clone())
                            .or_insert(Floor {
                                revision: 0,
                                stopped: None,
                            })
                            .stopped
                            .get_or_insert(*s);
                    }
                }
                run.stopping.remove(g);
            }
        }
        Ok(self.grants_view(&Native::routes()))
    }

    /// Lets the bosses here go on: only Desk on the Mac asks it. Written
    /// before it is in force.
    pub async fn clear_bosses_pause(&self, _: Native) -> Result<GrantsView, DeskError> {
        let _one = self.0.grant_write.lock().await;
        // Never while the grants it would let go on can't be read.
        self.grants_loaded().await?;
        let call = CallRow {
            caller: "desk".into(),
            boss: None,
            group: None,
            tool: "bosses.pause_clear",
            input: "{}".into(),
            input_hash: hash(b""),
            outcome: "ok",
            said: None,
            result: serde_json::json!({}),
        };
        self.0.store.put_bosses_paused(false, None, &call).await?;
        self.0.grants.lock().paused = None;
        Ok(self.grants_view(&Native::routes()))
    }

    /// Puts `id`, about to start here, in `group` by its grant: born in it.
    /// False where the grants don't say this engine's groups.
    pub(super) async fn born_in(&self, id: Uuid, group: &str) -> Result<bool, DeskError> {
        // Ungrouped is where a session is with no grant to say otherwise.
        if !self.stamps_from_grants() || group == UNGROUPED {
            return Ok(false);
        }
        let _one = self.0.grant_write.lock().await;
        self.grants_loaded().await?;
        let Some(g) = self.0.grants.lock().grants.get(group).cloned() else {
            return Err(DeskError::NotFound);
        };
        let call = CallRow {
            caller: "engine".into(),
            boss: None,
            group: Some(group.into()),
            tool: "grant.born",
            input: "{}".into(),
            input_hash: hash(id.as_bytes()),
            outcome: "ok",
            said: None,
            result: serde_json::json!({}),
        };
        self.commit_grants(vec![grant::with_born(&g, id)], None, call)
            .await?;
        Ok(true)
    }

    /// `id` didn't start: it is born in no grant.
    pub(super) async fn born_undo(&self, id: Uuid) {
        if !self.0.opts.bosses.machine {
            return;
        }
        let _one = self.0.grant_write.lock().await;
        let Some(mut g) = self
            .0
            .grants
            .lock()
            .grants
            .values()
            .find(|g| g.born.contains(&id))
            .cloned()
        else {
            return;
        };
        g.born.remove(&id);
        let call = CallRow {
            caller: "engine".into(),
            boss: None,
            group: Some(g.group.clone()),
            tool: "grant.unborn",
            input: "{}".into(),
            input_hash: hash(id.as_bytes()),
            outcome: "ok",
            said: None,
            result: serde_json::json!({}),
        };
        if let Err(e) = self.commit_grants(vec![g], None, call).await {
            tracing::warn!(%id, error = %e, "a session that didn't start stays born in its grant");
        }
    }

    /// Writes `writes` and `delete` with `call`'s audit; on a machine, with
    /// the sidebar its grants now make and each newly named session's first
    /// restamp. How many sessions were restamped.
    async fn commit_grants(
        &self,
        writes: Vec<Grant>,
        delete: Option<(&str, Option<Stopped>)>,
        call: CallRow,
    ) -> Result<usize, DeskError> {
        let mut next = self.0.grants.lock().grants.clone();
        for g in &writes {
            next.insert(g.group.clone(), g.clone());
        }
        let floor = delete.and_then(|(d, stopped)| {
            next.remove(d).map(|g| {
                (
                    d.to_string(),
                    Floor {
                        revision: g.revision,
                        stopped: g.stopped.or(stopped),
                    },
                )
            })
        });
        let keep = |run: &mut GrantsRun, next: BTreeMap<String, Grant>| {
            for g in &writes {
                run.floors.remove(&g.group);
            }
            if let Some((d, f)) = &floor {
                run.floors.insert(d.clone(), *f);
            }
            run.grants = next;
        };
        if !self.0.opts.bosses.machine {
            self.0
                .store
                .write_grants(&writes, delete, None, &call)
                .await?;
            keep(&mut self.0.grants.lock(), next);
            return Ok(0);
        }
        let one = self.0.layout_write.lock().await;
        let before = self.layout();
        let layout = grant::layout_of(next.values(), &before).checked_except(&Default::default())?;
        let named: Vec<(Uuid, String)> = grant::membership(writes.iter()).into_iter().collect();
        let all: Vec<(Uuid, String)> = grant::membership(next.values()).into_iter().collect();
        let granted: Vec<String> = next.keys().filter(|g| *g != UNGROUPED).cloned().collect();
        let written = self
            .0
            .store
            .write_grants(
                &writes,
                delete,
                Some(Projection {
                    layout: &layout,
                    named,
                    all,
                    granted,
                }),
                &call,
            )
            .await?;
        {
            let mut run = self.0.grants.lock();
            keep(&mut run, next);
            run.since.get_or_insert_with(Utc::now);
        }
        self.0.state.lock().layout = layout.clone();
        self.scopes_follow(&layout);
        self.boards_ungrouped(&written.ungrouped_boards).await;
        drop(one);
        self.announce(DeskEvent::Layout {
            layout: layout.clone(),
        });
        let ids: Vec<Uuid> = self.0.state.lock().records.keys().copied().collect();
        for id in ids {
            let item = session_item(id);
            let moved = before.group_of(&item).map(|g| (&g.id, &g.name))
                != layout.group_of(&item).map(|g| (&g.id, &g.name));
            if moved {
                self.publish(id);
            }
        }
        if !written.restamped.is_empty() {
            tracing::info!(
                sessions = written.restamped.len(),
                "sessions' rows restamped from this machine's old groups to their grants'"
            );
        }
        Ok(written.restamped.len())
    }
}
