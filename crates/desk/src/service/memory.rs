//! Session memory's share of the desk: whether each record is harvested,
//! where a record's harvest begins, the daily sweep, and what the person
//! reads of it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::engine::Engine;
use crate::ledger_store::Cursor;
use crate::memory::record::Harvested;
use crate::memory::{harvestable, Skip, Subject, Switches};
use crate::memory_store::{
    group_key, GroupTotals, KeptExchange, MemoryCounts, MemoryHit, MemoryQuery, PAUSED,
};
use crate::model::SessionRecord;

/// How often conversations past their keeping are swept.
pub(super) const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The most hits one search returns.
const SEARCH_MAX: i64 = 50;
/// The most exchanges a session's memory lists.
const LIST_MAX: i64 = 200;

/// What a read does with a record's conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Harvest {
    /// Harvest it from its own offset.
    On,
    /// Keep its offset where it is, so nothing is lost: there is no
    /// redactor yet to keep it safely.
    Hold,
    /// Never remember it: its offset follows the ledger's. Remembering
    /// turned off (for the install, the person's pause, the session, its
    /// group, its folder) means nothing said meanwhile is ever kept.
    Skip,
}

/// A keyword search's answer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearch {
    pub hits: Vec<MemoryHitView>,
    /// Only keywords were matched: the search index isn't ready.
    pub keyword_only: bool,
    /// What the person should know of the search, in plain words.
    pub notice: Option<&'static str>,
}

/// A hit as the person reads it, its session named.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHitView {
    #[serde(flatten)]
    pub hit: MemoryHit,
    /// The session's name, if Desk still has it.
    pub name: Option<String>,
    pub engine: Option<Engine>,
    /// Its group's name, as the sidebar calls it.
    pub group_name: Option<String>,
}

/// What Desk remembers of one session.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMemory {
    pub exchanges: Vec<KeptExchange>,
    pub counts: MemoryCounts,
    /// Whether what it says now is being remembered.
    pub remembering: bool,
    /// Why not, in plain words.
    pub why: Option<String>,
    /// What else the person should know of it, in plain words: that its
    /// oldest exchanges are past what the search index holds.
    pub notice: Option<String>,
    /// Its decisions, facts and to-dos still held, and whether Desk is
    /// noting them.
    pub notes: super::memory_derive::Notes,
    /// The session's own Remember switch; off once Desk no longer has it.
    pub memory_on: bool,
    /// Its engine keeps no record Desk can read: it remembers only what
    /// it's for.
    pub no_record: bool,
    /// When Desk last kept something it said.
    pub last_read: Option<DateTime<Utc>>,
    /// Desk no longer has the session: what it remembered is all there is,
    /// with no switch to turn.
    pub cleared: bool,
    /// The person's switches it stands under: the pause, and its group's.
    pub switches: SessionSwitches,
}

/// The switches one session's memory stands under, for its panel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSwitches {
    pub paused: bool,
    /// The sidebar group it is in now, with its switches; none when
    /// ungrouped.
    pub group: Option<GroupMemoryView>,
}

/// A group's memory switches (`memory.group.<id>`), as the person set them
/// or as they stand unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemory {
    /// Its sessions' conversations are remembered.
    pub harvest: bool,
    /// Desk's model may read them to note decisions, facts and to-dos.
    pub derive: bool,
    /// Desk asks before handing its work to another engine.
    pub handoff_ask: bool,
    /// The person decided whether Desk's model may read them (asked once).
    pub derive_decided: bool,
}

impl GroupMemory {
    /// As `value` (the stored setting, snake_case) says, else the defaults:
    /// remembered, not read by the model until the person lets it, asked
    /// before a handoff.
    fn of(value: Option<&Value>) -> Self {
        let flag = |k: &str| value.and_then(|v| v.get(k)).and_then(Value::as_bool);
        Self {
            harvest: flag("harvest").unwrap_or(true),
            derive: flag("derive").unwrap_or(false),
            handoff_ask: flag("handoff_ask").unwrap_or(true),
            derive_decided: flag("derive").is_some(),
        }
    }
}

/// A group's switches as a change left them, and what the person should
/// know of it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemoryChanged {
    #[serde(flatten)]
    pub switches: GroupMemory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<&'static str>,
}

/// Why remembering couldn't be turned on: a record couldn't be read to
/// pass over what was said while it was off.
pub const NOT_ON: &str = "Desk couldn't turn remembering on just now; try again.";
/// Why not, while Desk holds what it can't keep yet.
pub const HELD: &str = "Desk can't turn remembering on while it's holding conversations it can't keep yet (Desk Memory is full, or it can't hide secrets here). Try again once that's cleared.";
/// Turned off, though what one session said last couldn't be read first.
pub const OFF_UNREAD: &str = "Remembering is off, but Desk couldn't read what this session said last, so some of it may not have been kept.";
/// The same, of several sessions.
pub const SOME_UNREAD: &str = "Done, but Desk couldn't read what some sessions said last, so some of it may not have been kept.";

/// The person's change to a group's switches: what it names changes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupMemoryChange {
    pub harvest: Option<bool>,
    pub derive: Option<bool>,
    pub handoff_ask: Option<bool>,
}

/// The person's change to memory's own switches.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemorySettingsChange {
    pub paused: Option<bool>,
}

/// What the install's `[desk.memory]` sets, for Settings → Memory to show.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryLimits {
    pub enabled: bool,
    pub derive: bool,
    pub backfill_days: u32,
    pub retain_days: u32,
    pub max_disk_mb: u64,
    pub index_per_minute: u32,
    pub derive_calls_per_day: u32,
    /// Calls made today to note decisions.
    pub derive_calls_today: i64,
    /// Whose model writes the notes, as a session's notes say it.
    pub provider: Option<String>,
    /// The hub may write them on another provider when that one can't.
    pub fallback_providers: bool,
}

/// One group's memory, for Settings → Memory and the group's menu.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMemoryView {
    pub id: String,
    /// As the sidebar calls it.
    pub name: String,
    #[serde(flatten)]
    pub switches: GroupMemory,
}

/// The person's memory settings, the install's limits, and what is kept.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySettings {
    pub paused: bool,
    /// Every group Desk has, with its switches.
    pub groups: Vec<GroupMemoryView>,
    pub limits: MemoryLimits,
    /// What is kept, by group (`group` none: ungrouped sessions').
    pub totals: Vec<GroupTotals>,
    /// What the person should know of the change just made, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<&'static str>,
}

/// What the panel says of a session past its cap.
fn past_cap_notice(counts: &MemoryCounts) -> Option<String> {
    (counts.past_cap > 0).then(|| {
        format!(
            "Desk searches only this session's latest {} exchanges by meaning; its {} older ones are found by keyword.",
            thousands(crate::memory_store::SESSION_CAP as i64),
            thousands(counts.past_cap)
        )
    })
}

/// `n` with a space between each three digits, as Desk writes numbers.
fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    match n < 0 {
        true => format!("-{out}"),
        false => out,
    }
}

impl Desk {
    /// What a read does with each of `r`'s `records`: its own transcript is
    /// harvested where it may be; a sub-agent's record is its parent's step
    /// (the parent's call's result holds what it found), so its offset
    /// follows the ledger's.
    pub(super) async fn harvest_modes(
        &self,
        r: &SessionRecord,
        records: &[PathBuf],
    ) -> Vec<Harvest> {
        match self.why_not(r, records).await {
            Ok(()) => records
                .iter()
                .enumerate()
                .map(|(i, _)| match i {
                    0 => Harvest::On,
                    _ => Harvest::Skip,
                })
                .collect(),
            Err(Why::Hold(_)) => vec![Harvest::Hold; records.len()],
            Err(Why::Skip(_)) => vec![Harvest::Skip; records.len()],
        }
    }

    /// Whether `r`'s conversation is harvested now, and if not, why.
    async fn why_not(&self, r: &SessionRecord, records: &[PathBuf]) -> Result<(), Why> {
        if !self.0.opts.memory.enabled {
            return Err(Why::Skip(crate::memory::DISABLED.into()));
        }
        if self.0.ledger.redactor.is_none() {
            return Err(Why::Hold(crate::memory::NO_REDACTOR.into()));
        }
        if self.memory_full() {
            return Err(Why::Hold(crate::memory::FULL.into()));
        }
        if !matches!(r.engine, Engine::Claude | Engine::Codex) {
            return Err(Why::Skip(crate::memory::NO_RECORD.into()));
        }
        let group = self
            .group_of_member(&session_item(r.id))
            .filter(|g| g != UNGROUPED);
        let mut refs: Vec<String> = records
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        refs.extend(r.cli_session_id.clone());
        refs.extend(r.transcript.clone());
        let facts = self
            .0
            .store
            .harvest_facts(r.id, r.engine, &refs, group.as_deref())
            .await
            .map_err(|e| {
                tracing::warn!(session = %r.id, error = format!("{e:#}"), "couldn't read whether to remember a session");
                Why::Hold("Desk couldn't check whether to remember this session.".into())
            })?;
        let groups_off: HashSet<String> = match (&group, facts.group_off) {
            (Some(g), true) => [g.clone()].into(),
            _ => HashSet::new(),
        };
        let home = self.0.env.get("HOME").cloned();
        let source = records
            .first()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| r.transcript.clone())
            .unwrap_or_default();
        let subject = Subject {
            engine: r.engine,
            cwd: &r.cwd,
            name: &r.name,
            cli_session_id: r.cli_session_id.as_deref(),
            source: &source,
            memory_on: facts.memory_on,
            group: group.as_deref(),
        };
        let switches = Switches {
            paused: facts.paused,
            groups_off: &groups_off,
            exclude: &self.0.opts.memory.exclude,
            home: home.as_deref(),
            forgotten: &facts.forgotten,
        };
        harvestable(&subject, &self.0.opts.hub_internal, &switches)
            .map_err(|skip| Why::Skip(skip.plain().into()))
    }

    /// `cursor` with its harvest's beginning decided, the first time it is
    /// harvested: from the record's start when its engine wrote it within
    /// `backfill_days`, else from its end now (older history is never
    /// harvested).
    pub(super) fn harvest_begins(&self, cursor: &Cursor, path: &Path, mode: Harvest) -> Cursor {
        let mut h = Harvested::of(&cursor.harvest);
        if mode != Harvest::On || h.decided {
            return cursor.clone();
        }
        h.decided = true;
        let mut cursor = cursor.clone();
        let since = std::time::SystemTime::now()
            - Duration::from_secs(u64::from(self.0.opts.memory.backfill_days) * 24 * 60 * 60);
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.modified().is_ok_and(|m| m < since) {
                cursor.harvested_to = meta.len().max(cursor.read_to);
            }
        }
        cursor.harvest = h.value();
        cursor
    }

    /// Deletes conversations past their keeping.
    pub(super) async fn sweep_memory(&self) {
        match self
            .0
            .store
            .sweep_memory(self.0.opts.memory.retain_days)
            .await
        {
            Ok(swept) if swept == Default::default() => {}
            Ok(swept) => {
                tracing::info!(
                    gone = swept.gone,
                    capped = swept.capped,
                    purges = swept.purges,
                    "swept conversations past their keeping"
                );
                self.memory_index_poke();
                self.measure_memory().await;
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "couldn't sweep old conversations"),
        }
    }

    /// The person's search of what Desk remembers: every session's, or one
    /// group's or one session's. By meaning and words once the search
    /// index is ready; until then by keyword only, and the answer says so.
    pub async fn memory_search(&self, q: &MemoryQuery) -> Result<MemorySearch, DeskError> {
        let found = self
            .memory_recall(
                &crate::memory::reach::Reader::Person,
                &super::memory_recall::Recall {
                    q: q.q.clone(),
                    group: q.group.clone(),
                    session: q.session,
                    kinds: q.kinds.clone(),
                    limit: usize::try_from(q.limit.clamp(1, SEARCH_MAX)).unwrap_or(1),
                    tool: "search",
                    ..Default::default()
                },
            )
            .await?;
        let hits: Vec<MemoryHit> = found
            .hits
            .into_iter()
            .map(|(r, _)| MemoryHit {
                kind: match (&r.item, r.compaction) {
                    (Some(i), _) => i.kind.as_str(),
                    (None, true) => "compaction",
                    (None, false) => "exchange",
                },
                noted_by_desk: r.item.as_ref().is_some_and(|i| i.noted),
                confirmed: r.item.as_ref().is_some_and(|i| i.confirmed),
                superseded: r.item.as_ref().is_some_and(|i| i.superseded),
                session: r.session,
                group: r.group,
                project: r.project,
                at: r.at,
                asker: r.asker,
                ask: r.ask,
                answer: r.answer,
                compaction: r.compaction,
            })
            .collect();
        let ids: Vec<Uuid> = hits
            .iter()
            .map(|h| h.session)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut named = std::collections::HashMap::new();
        for id in &ids {
            if let Ok(r) = self.record(*id) {
                named.insert(
                    *id,
                    (r.display_name.clone().unwrap_or(r.name.clone()), r.engine),
                );
            }
        }
        let missing: Vec<Uuid> = ids
            .into_iter()
            .filter(|id| !named.contains_key(id))
            .collect();
        if !missing.is_empty() {
            // A session cleared away is named as its memory remembers it.
            for (id, name, engine) in self.0.store.memory_names(&missing).await? {
                named.insert(id, (name, engine));
            }
            for r in self.0.store.sessions_of(&missing).await? {
                named.insert(
                    r.id,
                    (r.display_name.clone().unwrap_or(r.name.clone()), r.engine),
                );
            }
        }
        Ok(MemorySearch {
            hits: hits
                .into_iter()
                .map(|hit| {
                    let (name, engine) = named.get(&hit.session).cloned().unzip();
                    let group_name = hit.group.as_deref().and_then(|g| self.group_name(g));
                    MemoryHitView {
                        hit: MemoryHit {
                            ask: crate::memory::record::clip(&hit.ask, 400),
                            answer: crate::memory::record::clip(&hit.answer, 400),
                            ..hit
                        },
                        name,
                        engine,
                        group_name,
                    }
                })
                .collect(),
            keyword_only: found.keyword_only,
            notice: found.notice,
        })
    }

    /// What Desk remembers of session `id`: its latest exchanges, its
    /// counts, and whether it is remembering now.
    pub async fn session_memory(&self, id: Uuid) -> Result<SessionMemory, DeskError> {
        let Some(r) = self.session_record_any(id).await else {
            // Cleared away: what it remembered stays, to read or forget.
            let (exchanges, counts) = self.0.store.session_memory(id, LIST_MAX).await?;
            if self.0.store.memory_names(&[id]).await?.is_empty() && counts.exchanges == 0 {
                return Err(DeskError::NotFound);
            }
            let group = self.0.store.latest_group(id).await?;
            return Ok(SessionMemory {
                notes: self.session_notes(id, group.as_deref()).await?,
                exchanges,
                notice: past_cap_notice(&counts),
                counts,
                remembering: false,
                why: Some(crate::memory::CLEARED.into()),
                memory_on: false,
                no_record: false,
                cleared: true,
                last_read: self.0.store.memory_last_kept(id).await?,
                switches: self.session_switches(None).await?,
            });
        };
        let (exchanges, counts) = self.0.store.session_memory(id, LIST_MAX).await?;
        let why = if self.0.opts.hub_internal.holds(Path::new(&r.cwd))
            || crate::ledger::agent_turn(&r.cwd, &r.name)
        {
            Some(Skip::HubInternal.plain().to_string())
        } else {
            let records = {
                let desk = self.clone();
                let r = r.clone();
                tokio::task::spawn_blocking(move || desk.records_of(&r))
                    .await
                    .unwrap_or_default()
            };
            self.why_not(&r, &records).await.err().map(Why::said)
        };
        Ok(SessionMemory {
            notes: self
                .session_notes(id, self.notes_group(id).as_deref())
                .await?,
            exchanges,
            notice: past_cap_notice(&counts),
            counts,
            remembering: why.is_none(),
            why,
            memory_on: self.0.store.session_memory_on(id).await?.unwrap_or(true),
            no_record: !matches!(r.engine, Engine::Claude | Engine::Codex),
            cleared: false,
            last_read: self.0.store.memory_last_kept(id).await?,
            switches: self.session_switches(self.notes_group(id)).await?,
        })
    }

    /// The pause, and `group`'s switches: a light read of two settings.
    async fn session_switches(&self, group: Option<String>) -> Result<SessionSwitches, DeskError> {
        let paused = self
            .0
            .store
            .setting(PAUSED)
            .await?
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let group = match group {
            Some(id) => {
                let set = self.0.store.setting(&group_key(&id)).await?;
                self.group_name(&id).map(|name| GroupMemoryView {
                    switches: GroupMemory::of(set.as_ref()),
                    id,
                    name,
                })
            }
            None => None,
        };
        Ok(SessionSwitches { paused, group })
    }

    /// Every session the registry keeps, closed ones too, and of them
    /// those in `group`.
    async fn sessions_in(&self, group: Option<&str>) -> Result<Vec<Uuid>, DeskError> {
        let mut ids: Vec<Uuid> = self.0.state.lock().records.keys().copied().collect();
        for r in self.0.store.every_session().await? {
            if !ids.contains(&r.id) {
                ids.push(r.id);
            }
        }
        Ok(match group {
            None => ids,
            Some(g) => ids
                .into_iter()
                .filter(|id| self.group_of_member(&session_item(*id)).as_deref() == Some(g))
                .collect(),
        })
    }

    /// Reads `ids`' records before a switch turns their remembering on or
    /// off; the caller holds the ledger's reads from before it read the
    /// switch until it has written it. Turning on, what was said while it
    /// was off is passed over to the records' ends first, so none of it is
    /// ever kept, and a record that can't be passed over refuses the switch;
    /// turning off, what was said until now is kept first, and what may not
    /// have been is said.
    async fn switch_reads(
        &self,
        ids: &[Uuid],
        turning: Option<bool>,
        unread: &'static str,
    ) -> Result<Option<&'static str>, DeskError> {
        match turning {
            None => Ok(None),
            Some(true) => {
                // Held text was said while it was on: passing over the
                // records would lose it with the rest.
                if self.0.ledger.redactor.is_none() || self.memory_full() {
                    return Err(DeskError::Conflict(HELD.into()));
                }
                for id in ids {
                    if self
                        .read_spend_held(*id, Some(Harvest::Skip))
                        .await
                        .is_err()
                    {
                        return Err(DeskError::Conflict(NOT_ON.into()));
                    }
                }
                Ok(None)
            }
            Some(false) => {
                let mut failed = false;
                for id in ids {
                    failed |= self.read_spend_held(*id, None).await.is_err();
                }
                Ok(failed.then_some(unread))
            }
        }
    }

    /// Runs `work` on its own task, so a request that goes away can't leave
    /// a switch half changed; what it answers.
    async fn switch_apart<T: Send + 'static>(
        &self,
        work: impl std::future::Future<Output = Result<T, DeskError>> + Send + 'static,
    ) -> Result<T, DeskError> {
        tokio::spawn(work).await.map_err(|e| {
            DeskError::Internal(eyre::eyre!("a memory switch's change stopped: {e}"))
        })?
    }

    /// Turns session `id`'s own Remember switch on or off. Off, nothing it
    /// says meanwhile is ever kept; what it remembered already stays until
    /// it is forgotten. What the person should know of it, if anything.
    pub async fn set_session_memory(
        &self,
        id: Uuid,
        on: bool,
    ) -> Result<Option<&'static str>, DeskError> {
        let d = self.clone();
        self.switch_apart(async move {
            let notice = {
                let _reading = d.0.ledger.reading.lock().await;
                let Some(was) = d.0.store.session_memory_on(id).await? else {
                    return Err(DeskError::NotFound);
                };
                let notice = d
                    .switch_reads(&[id], (was != on).then_some(on), OFF_UNREAD)
                    .await?;
                if !d.0.store.set_session_memory(id, on).await? {
                    return Err(DeskError::NotFound);
                }
                notice
            };
            d.announce(crate::model::DeskEvent::Setting {
                key: format!("memory.session.{id}"),
            });
            d.derive_poke();
            Ok(notice)
        })
        .await
    }

    /// The person's memory settings, the install's limits, and what is
    /// kept.
    pub async fn memory_settings(&self) -> Result<MemorySettings, DeskError> {
        let set = self.0.store.memory_settings().await?;
        let mut groups: Vec<GroupMemoryView> = self
            .group_names()
            .into_iter()
            .map(|(id, name)| GroupMemoryView {
                switches: GroupMemory::of(set.get(&group_key(&id))),
                id,
                name,
            })
            .collect();
        groups.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let m = &self.0.opts.memory;
        Ok(MemorySettings {
            paused: set.get(PAUSED).and_then(Value::as_bool).unwrap_or(false),
            groups,
            limits: MemoryLimits {
                enabled: m.enabled,
                derive: m.derive,
                backfill_days: m.backfill_days,
                retain_days: m.retain_days,
                max_disk_mb: m.max_disk_mb,
                index_per_minute: m.index_per_minute,
                derive_calls_per_day: m.derive_calls_per_day,
                derive_calls_today: self.0.store.derive_calls(self.derive_day()).await?,
                provider: self
                    .0
                    .derive
                    .deriver
                    .as_ref()
                    .and_then(|d| d.written_by().provider),
                fallback_providers: self
                    .0
                    .derive
                    .deriver
                    .as_ref()
                    .is_some_and(|d| d.written_by().fallback),
            },
            totals: self.0.store.memory_totals().await?,
            notice: None,
        })
    }

    /// The person pauses or resumes remembering for every session. Paused,
    /// nothing said meanwhile is ever kept.
    pub async fn change_memory_settings(
        &self,
        change: &MemorySettingsChange,
    ) -> Result<MemorySettings, DeskError> {
        let mut notice = None;
        if let Some(paused) = change.paused {
            let d = self.clone();
            notice = self
                .switch_apart(async move {
                    let notice = {
                        let _reading = d.0.ledger.reading.lock().await;
                        let was =
                            d.0.store
                                .setting(PAUSED)
                                .await?
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                        let ids = d.sessions_in(None).await?;
                        let notice = d
                            .switch_reads(&ids, (was != paused).then_some(!paused), SOME_UNREAD)
                            .await?;
                        d.0.store
                            .put_setting(PAUSED, &serde_json::json!(paused))
                            .await?;
                        notice
                    };
                    d.announce(crate::model::DeskEvent::Setting { key: PAUSED.into() });
                    d.derive_poke();
                    Ok(notice)
                })
                .await?;
        }
        Ok(MemorySettings {
            notice,
            ..self.memory_settings().await?
        })
    }

    /// The person changes group `id`'s memory switches: what the change
    /// names, the rest kept.
    pub async fn change_group_memory(
        &self,
        id: &str,
        change: &GroupMemoryChange,
    ) -> Result<GroupMemoryChanged, DeskError> {
        if self.group_name(id).is_none() {
            return Err(DeskError::NotFound);
        }
        let m = &self.0.opts.memory;
        if change.derive == Some(true) && !(m.enabled && m.derive) {
            return Err(DeskError::Invalid(
                "Desk's model can't note decisions here: noting is turned off for this Desk in the hub's config ([desk.memory] derive).".into(),
            ));
        }
        let key = group_key(id);
        let mut fields = serde_json::Map::new();
        for (k, v) in [
            ("harvest", change.harvest),
            ("derive", change.derive),
            ("handoff_ask", change.handoff_ask),
        ] {
            if let Some(v) = v {
                fields.insert(k.into(), Value::Bool(v));
            }
        }
        let (d, id, harvest) = (self.clone(), id.to_string(), change.harvest);
        self.switch_apart(async move {
            let (value, notice) = {
                let _reading = d.0.ledger.reading.lock().await;
                let was = GroupMemory::of(d.0.store.setting(&key).await?.as_ref());
                let ids = d.sessions_in(Some(&id)).await?;
                let notice = d
                    .switch_reads(&ids, harvest.filter(|h| *h != was.harvest), SOME_UNREAD)
                    .await?;
                (d.0.store.merge_setting(&key, &fields).await?, notice)
            };
            d.announce(crate::model::DeskEvent::Setting { key });
            d.derive_poke();
            Ok(GroupMemoryChanged {
                switches: GroupMemory::of(Some(&value)),
                notice,
            })
        })
        .await
    }

    /// Forgets what Desk remembers of session `id`, whether or not Desk
    /// still has the session: its exchanges and notes go, and its
    /// conversation, records and exchanges are never harvested again.
    pub async fn forget_memory(&self, id: Uuid) -> Result<(), DeskError> {
        match self.0.store.forget_memory(id).await? {
            true => {
                // Its gist goes too; its turns are the Floor's.
                self.log_turns(vec![crate::floor::TurnOp::ForgetGists {
                    subjects: vec![crate::floor::SubjectKey::session(id)],
                }]);
                self.memory_index_poke();
                self.measure_memory().await;
                Ok(())
            }
            false => Err(DeskError::NotFound),
        }
    }

    /// The registry's record of `id`, closed sessions too.
    async fn session_record_any(&self, id: Uuid) -> Option<SessionRecord> {
        if let Ok(r) = self.record(id) {
            return Some(r);
        }
        self.0.store.get(id).await.ok().flatten()
    }
}

/// Why a session's conversation isn't harvested now.
enum Why {
    /// Its offset holds: nothing it says is lost.
    Hold(String),
    /// Never to be remembered.
    Skip(String),
}

impl Why {
    fn said(self) -> String {
        match self {
            Why::Hold(s) | Why::Skip(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::tests::Plain;
    use crate::store::{build_pool, tests::record_fixture, DeskStore};
    use std::sync::Arc;
    use workforce_postgres::test_support::with_test_pool;

    const CLAUDE: &str = concat!(
        include_str!("../../fixtures/ledger/claude.jsonl"),
        include_str!("../../fixtures/memory/claude-more.jsonl")
    );

    async fn cursor_of(d: &Desk, id: Uuid) -> Option<Cursor> {
        let r = d.session_record_any(id).await?;
        let source = r.transcript.clone()?;
        d.0.store.ledger_cursor(id, &source).await.ok().flatten()
    }

    /// Waits until `id`'s record has been read to its end by the ledger.
    async fn read_through(d: &Desk, id: Uuid) -> Cursor {
        read_to_end(d, id).await.expect("the record was read")
    }

    async fn read_to_end(d: &Desk, id: Uuid) -> Option<Cursor> {
        for _ in 0..400 {
            if let Some(c) = cursor_of(d, id).await {
                let len = std::fs::metadata(d.session_record_any(id).await?.transcript?)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if c.read_to == len {
                    return Some(c);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        None
    }

    #[tokio::test]
    async fn without_a_redactor_nothing_is_lost_and_while_paused_nothing_is_kept() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let put = |name: &str| {
                let t = dir.path().join(format!("{name}.jsonl"));
                // Each its own conversation: an exchange is kept once, by the
                // first session that read it.
                std::fs::write(
                    &t,
                    CLAUDE.replace("\"uuid\":\"", &format!("\"uuid\":\"{name}-")),
                )
                .unwrap();
                let mut r = record_fixture(name);
                r.cwd = dir.path().to_string_lossy().into_owned();
                r.transcript = Some(t.to_string_lossy().into_owned());
                r.state = crate::model::SessionState::Closed;
                r
            };
            let bare = put("acme-bare");
            // An old record: its engine last wrote it before the backfill.
            let old = put("acme-old");
            std::fs::File::options()
                .write(true)
                .open(old.transcript.as_deref().unwrap())
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(90 * 24 * 3600))
                .unwrap();
            for r in [&bare, &old] {
                store.put(r).await.unwrap();
            }
            let start = |floor: super::super::Floor| {
                let cancel = tokio_util::sync::CancellationToken::new();
                let pool = pool.clone();
                let dir = dir.path().to_path_buf();
                async move {
                    let d = Desk::start(
                        DeskStore::from_pool(pool),
                        crate::testing::options(&dir),
                        floor,
                        cancel.clone(),
                    )
                    .await
                    .unwrap();
                    (d, cancel)
                }
            };
            let floor = || super::super::Floor {
                redactor: Some(Arc::new(Plain)),
                ..Default::default()
            };
            let until_harvested = |d: Desk, id: Uuid| async move {
                for _ in 0..400 {
                    let c = cursor_of(&d, id).await;
                    if c.as_ref()
                        .is_some_and(|c| c.harvested_to > 0 && c.harvested_to == c.read_to)
                    {
                        return c.unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                panic!("the harvest never caught up");
            };

            // No redactor: the ledger reads on, the harvest holds.
            let (d, cancel) = start(Default::default()).await;
            let c = read_through(&d, bare.id).await;
            assert_eq!(c.harvested_to, 0);
            let m = d.session_memory(bare.id).await.unwrap();
            assert_eq!(
                (m.remembering, m.why.as_deref()),
                (false, Some(crate::memory::NO_REDACTOR))
            );
            cancel.cancel();

            // With one, what was held is harvested; the old record's history
            // never is.
            let (d, cancel) = start(floor()).await;
            until_harvested(d.clone(), bare.id).await;
            let m = d.session_memory(bare.id).await.unwrap();
            assert_eq!(
                m.counts.exchanges, 6,
                "nothing held for a redactor was lost"
            );
            assert!(m.remembering);
            let c = until_harvested(d.clone(), old.id).await;
            assert_eq!(c.harvested_to, c.read_to);
            assert_eq!(
                d.session_memory(old.id).await.unwrap().counts.exchanges,
                0,
                "older than backfill_days"
            );
            cancel.cancel();

            // Paused: the harvest goes on with the ledger, and nothing said
            // meanwhile is kept, then or after.
            store
                .put_setting(crate::memory_store::PAUSED, &serde_json::json!(true))
                .await
                .unwrap();
            let paused = put("acme-paused");
            store.put(&paused).await.unwrap();
            let (d, cancel) = start(floor()).await;
            until_harvested(d.clone(), paused.id).await;
            let m = d.session_memory(paused.id).await.unwrap();
            assert_eq!(
                (m.counts.exchanges, m.why.as_deref()),
                (0, Some(Skip::Paused.plain()))
            );
            cancel.cancel();
            store
                .put_setting(crate::memory_store::PAUSED, &serde_json::json!(false))
                .await
                .unwrap();
            let (d, _cancel) = start(floor()).await;
            d.spend_read(paused.id);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let m = d.session_memory(paused.id).await.unwrap();
            assert_eq!(m.counts.exchanges, 0, "nothing said while paused is kept");
            assert!(m.remembering);
        })
        .await;
    }

    #[tokio::test]
    async fn a_turns_end_closes_the_exchange_it_left_open() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let t = dir.path().join("web.jsonl");
            // It ends on a reply its record never says stopped.
            std::fs::write(&t, &CLAUDE[..CLAUDE.find("\"uuid\":\"u-11\"").unwrap() - 1]).unwrap();
            let mut r = record_fixture("acme-web");
            r.cwd = dir.path().to_string_lossy().into_owned();
            r.transcript = Some(t.to_string_lossy().into_owned());
            r.state = crate::model::SessionState::Closed;
            store.put(&r).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                super::super::Floor {
                    redactor: Some(Arc::new(Plain)),
                    ..Default::default()
                },
                Default::default(),
            )
            .await
            .unwrap();
            let counts = |d: Desk| async move { d.session_memory(r.id).await.unwrap().counts };
            for _ in 0..400 {
                if counts(d.clone()).await.open == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(counts(d.clone()).await.open, 1);
            d.spend_read_ending(r.id);
            for _ in 0..400 {
                if counts(d.clone()).await.open == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let m = d.session_memory(r.id).await.unwrap();
            assert_eq!((m.counts.open, m.counts.exchanges), (0, 5));
            assert!(m.exchanges.last().unwrap().ended.is_some());
        })
        .await;
    }

    /// Adds a copy of the fixture's conversation to `path`, its exchanges
    /// told apart by `tag`.
    fn say(path: &str, tag: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(
                CLAUDE
                    .replace("\"uuid\":\"", &format!("\"uuid\":\"{tag}-"))
                    .as_bytes(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn what_is_said_while_remembering_is_off_is_never_kept_once_it_is_on_again() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let put = |name: &str| {
                let t = dir.path().join(format!("{name}.jsonl"));
                std::fs::write(&t, "").unwrap();
                say(&t.to_string_lossy(), name);
                let mut r = record_fixture(name);
                r.cwd = dir.path().to_string_lossy().into_owned();
                r.transcript = Some(t.to_string_lossy().into_owned());
                r.state = crate::model::SessionState::Closed;
                r
            };
            // Its own switch, the pause, and its group's harvest.
            let (own, paused, grouped) = (put("acme-own"), put("acme-paused"), put("acme-grouped"));
            for r in [&own, &paused, &grouped] {
                store.put(r).await.unwrap();
            }
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                super::super::Floor {
                    redactor: Some(Arc::new(Plain)),
                    ..Default::default()
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            d.change_layout(|l| l.create("g-acme", "Acme", &[session_item(grouped.id)], None))
                .await
                .unwrap();
            let kept = |id: Uuid| {
                let d = d.clone();
                async move { d.session_memory(id).await.unwrap().counts.exchanges }
            };
            let until = |id: Uuid, n: i64| {
                let d = d.clone();
                async move {
                    for _ in 0..400 {
                        if d.session_memory(id).await.unwrap().counts.exchanges == n {
                            return;
                        }
                        d.spend_read(id);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    panic!("never {n} exchanges");
                }
            };
            for r in [&own, &paused, &grouped] {
                until(r.id, 6).await;
            }
            let path = |r: &SessionRecord| r.transcript.clone().unwrap();

            // Off, said, on again: with no read between, what was said
            // while it was off is passed over.
            d.set_session_memory(own.id, false).await.unwrap();
            say(&path(&own), "own-off");
            d.set_session_memory(own.id, true).await.unwrap();

            let pause = |on: bool| MemorySettingsChange { paused: Some(on) };
            d.change_memory_settings(&pause(true)).await.unwrap();
            say(&path(&paused), "paused-off");
            d.change_memory_settings(&pause(false)).await.unwrap();

            let harvest = |on: bool| GroupMemoryChange {
                harvest: Some(on),
                ..Default::default()
            };
            d.change_group_memory("g-acme", &harvest(false))
                .await
                .unwrap();
            say(&path(&grouped), "grouped-off");
            d.change_group_memory("g-acme", &harvest(true))
                .await
                .unwrap();

            for r in [&own, &paused, &grouped] {
                d.spend_read(r.id);
                read_through(&d, r.id).await;
                let m = d.session_memory(r.id).await.unwrap();
                assert_eq!(m.counts.exchanges, 6, "{}: nothing said while off", r.name);
                assert_eq!(
                    m.counts.open, 0,
                    "{}: what was open is closed, not joined",
                    r.name
                );
            }

            // What is said once it is on again is kept.
            for r in [&own, &paused, &grouped] {
                say(&path(r), &format!("{}-on", r.name));
                until(r.id, 12).await;
            }
            assert_eq!(kept(own.id).await, 12);
        })
        .await;
    }

    #[tokio::test]
    async fn turning_remembering_off_keeps_what_was_said_until_then() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let t = dir.path().join("acme-web.jsonl");
            std::fs::write(&t, "").unwrap();
            say(&t.to_string_lossy(), "first");
            let mut r = record_fixture("acme-web");
            r.cwd = dir.path().to_string_lossy().into_owned();
            r.transcript = Some(t.to_string_lossy().into_owned());
            r.state = crate::model::SessionState::Closed;
            store.put(&r).await.unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                super::super::Floor {
                    redactor: Some(Arc::new(Plain)),
                    ..Default::default()
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            read_through(&d, r.id).await;
            for _ in 0..400 {
                if d.session_memory(r.id).await.unwrap().counts.exchanges == 6 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            // Said while on, with no read yet: turning off reads it first.
            say(&t.to_string_lossy(), "before-off");
            d.set_session_memory(r.id, false).await.unwrap();
            assert_eq!(d.session_memory(r.id).await.unwrap().counts.exchanges, 12);
        })
        .await;
    }

    #[tokio::test]
    async fn forgetting_what_desk_remembers_takes_the_sessions_gist() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool.clone());
            let t = dir.path().join("acme-web.jsonl");
            std::fs::write(&t, "").unwrap();
            say(&t.to_string_lossy(), "gist");
            let mut r = record_fixture("acme-web");
            r.cwd = dir.path().to_string_lossy().into_owned();
            r.transcript = Some(t.to_string_lossy().into_owned());
            r.state = crate::model::SessionState::Closed;
            store.put(&r).await.unwrap();
            let logged = Arc::new(super::super::tests::Logged::default());
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                super::super::Floor {
                    turns: Some(logged.clone()),
                    redactor: Some(Arc::new(Plain)),
                    ..Default::default()
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            read_through(&d, r.id).await;
            d.forget_memory(r.id).await.unwrap();
            let id = r.id;
            assert!(
                super::super::tests::logs(&logged, |ops| ops.iter().any(|op| matches!(
                    op,
                    crate::floor::TurnOp::ForgetGists { subjects }
                        if subjects == &[crate::floor::SubjectKey::session(id)]
                ) && !ops
                    .iter()
                    .any(|op| matches!(op, crate::floor::TurnOp::Forget { .. }))))
                .await,
                "its gist goes, its turns stay"
            );
        })
        .await;
    }

    /// A closed session of `name` whose record is the fixture's
    /// conversation, kept; and a desk started on it, read through.
    async fn started(
        pool: &workforce_postgres::PostgresPool,
        dir: &std::path::Path,
        name: &str,
        redactor: bool,
    ) -> (Desk, SessionRecord) {
        DeskStore::migrate(pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let t = dir.join(format!("{name}.jsonl"));
        std::fs::write(&t, "").unwrap();
        say(&t.to_string_lossy(), name);
        let mut r = record_fixture(name);
        r.cwd = dir.to_string_lossy().into_owned();
        r.transcript = Some(t.to_string_lossy().into_owned());
        r.state = crate::model::SessionState::Closed;
        store.put(&r).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool.clone()),
            crate::testing::options(dir),
            super::super::Floor {
                redactor: redactor.then(|| Arc::new(Plain) as Arc<dyn crate::memory::Redactor>),
                ..Default::default()
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        read_through(&d, r.id).await;
        (d, r)
    }

    /// Makes `path` unreadable, or readable again.
    fn readable(path: &str, yes: bool) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if yes { 0o644 } else { 0o000 }),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_switch_turned_on_fails_closed_when_what_was_said_meanwhile_cant_be_passed_over() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, r) = started(&pool, dir.path(), "acme-web", true).await;
            let path = r.transcript.clone().unwrap();
            d.set_session_memory(r.id, false).await.unwrap();

            // Unread, but it hasn't grown: there is nothing to pass over.
            readable(&path, false);
            assert_eq!(d.set_session_memory(r.id, true).await.unwrap(), None);
            d.set_session_memory(r.id, false).await.unwrap();

            // It grew, and can't be read: remembering stays off.
            readable(&path, true);
            say(&path, "off");
            readable(&path, false);
            match d.set_session_memory(r.id, true).await {
                Err(DeskError::Conflict(why)) => assert_eq!(why, NOT_ON),
                other => panic!("turned on: {other:?}"),
            }
            assert!(!d.session_memory(r.id).await.unwrap().memory_on);

            // Resuming the pause fails closed the same way.
            readable(&path, true);
            d.set_session_memory(r.id, true).await.unwrap();
            d.change_memory_settings(&MemorySettingsChange { paused: Some(true) })
                .await
                .unwrap();
            say(&path, "paused");
            readable(&path, false);
            match d
                .change_memory_settings(&MemorySettingsChange {
                    paused: Some(false),
                })
                .await
            {
                Err(DeskError::Conflict(why)) => assert_eq!(why, NOT_ON),
                other => panic!("resumed: {other:?}"),
            }
            assert_eq!(
                d.0.store.setting(PAUSED).await.unwrap(),
                Some(serde_json::json!(true))
            );
            readable(&path, true);
        })
        .await;
    }

    #[tokio::test]
    async fn turning_off_when_the_last_words_cant_be_read_still_turns_off_and_says_so() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, r) = started(&pool, dir.path(), "acme-web", true).await;
            let path = r.transcript.clone().unwrap();
            say(&path, "last");
            readable(&path, false);
            assert_eq!(
                d.set_session_memory(r.id, false).await.unwrap(),
                Some(OFF_UNREAD)
            );
            assert!(!d.session_memory(r.id).await.unwrap().memory_on);
            readable(&path, true);
        })
        .await;
    }

    #[tokio::test]
    async fn remembering_isnt_turned_on_while_desk_holds_what_it_cant_keep() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            // No redactor: what is said is held, not kept.
            let (d, r) = started(&pool, dir.path(), "acme-web", false).await;
            d.set_session_memory(r.id, false).await.unwrap();
            match d.set_session_memory(r.id, true).await {
                Err(DeskError::Conflict(why)) => assert_eq!(why, HELD),
                other => panic!("turned on: {other:?}"),
            }
            let c = cursor_of(&d, r.id).await.unwrap();
            assert_eq!(c.harvested_to, 0, "what was held is still held");
        })
        .await;
    }

    #[tokio::test]
    async fn switches_changed_at_once_each_see_the_other_and_nothing_said_while_off_is_kept() {
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (d, r) = started(&pool, dir.path(), "acme-web", true).await;
            let path = r.transcript.clone().unwrap();
            for _ in 0..400 {
                if d.session_memory(r.id).await.unwrap().counts.exchanges == 6 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            d.set_session_memory(r.id, false).await.unwrap();
            say(&path, "off");
            // On and off at once, many times: each reads the switch as the
            // other left it.
            for _ in 0..5 {
                let (on, off) = tokio::join!(
                    d.set_session_memory(r.id, true),
                    d.set_session_memory(r.id, false)
                );
                on.unwrap();
                off.unwrap();
            }
            d.set_session_memory(r.id, true).await.unwrap();
            d.spend_read(r.id);
            read_through(&d, r.id).await;
            assert_eq!(
                d.session_memory(r.id).await.unwrap().counts.exchanges,
                6,
                "nothing said while it was off"
            );
        })
        .await;
    }
}
