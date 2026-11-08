//! Session memory's share of the desk: whether each record is harvested,
//! where a record's harvest begins, the daily sweep, and what the person
//! reads of it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::engine::Engine;
use crate::ledger_store::Cursor;
use crate::memory::record::Harvested;
use crate::memory::{harvestable, Skip, Subject, Switches};
use crate::memory_store::{KeptExchange, MemoryCounts, MemoryHit, MemoryQuery};
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
        })
    }

    /// Forgets what Desk remembers of session `id`, whether or not Desk
    /// still has the session: its exchanges and notes go, and its
    /// conversation, records and exchanges are never harvested again.
    pub async fn forget_memory(&self, id: Uuid) -> Result<(), DeskError> {
        match self.0.store.forget_memory(id).await? {
            true => {
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
}
