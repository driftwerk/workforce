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
    /// Keep its offset where it is, so nothing is lost: no redactor,
    /// harvesting turned off for the install, or paused.
    Hold,
    /// Never remember it: its offset follows the ledger's.
    Skip,
}

/// A keyword search's answer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearch {
    pub hits: Vec<MemoryHitView>,
    /// Only keywords were matched: the search index isn't ready.
    pub keyword_only: bool,
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
            return Err(Why::Hold(crate::memory::DISABLED.into()));
        }
        if self.0.ledger.redactor.is_none() {
            return Err(Why::Hold(crate::memory::NO_REDACTOR.into()));
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
            pid: r.external_pid,
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
            procs: &[],
        };
        match harvestable(&subject, &self.0.opts.hub_internal, &switches) {
            Ok(()) => Ok(()),
            Err(Skip::Paused) => Err(Why::Hold(Skip::Paused.plain().into())),
            Err(skip) => Err(Why::Skip(skip.plain().into())),
        }
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
            Ok(0) => {}
            Ok(gone) => tracing::info!(gone, "swept conversations past their keeping"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "couldn't sweep old conversations"),
        }
    }

    /// The person's keyword search of what Desk remembers: every session's,
    /// or one group's or one session's. Until the search index is ready,
    /// only keywords are matched, and the answer says so.
    pub async fn memory_search(&self, q: &MemoryQuery) -> Result<MemorySearch, DeskError> {
        if q.q.trim().is_empty() {
            return Err(DeskError::Invalid("Say what to search for.".into()));
        }
        let q = MemoryQuery {
            limit: q.limit.clamp(1, SEARCH_MAX),
            ..q.clone()
        };
        let hits = self.0.store.memory_search(&q).await?;
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
            keyword_only: true,
            notice: Some(crate::memory::KEYWORD_ONLY),
        })
    }

    /// What Desk remembers of session `id`: its latest exchanges, its
    /// counts, and whether it is remembering now.
    pub async fn session_memory(&self, id: Uuid) -> Result<SessionMemory, DeskError> {
        let r = self
            .session_record_any(id)
            .await
            .ok_or(DeskError::NotFound)?;
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
            exchanges,
            counts,
            remembering: why.is_none(),
            why,
        })
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
    async fn without_a_redactor_or_while_paused_nothing_is_harvested_and_nothing_lost() {
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
            let (bare, paused) = (put("acme-bare"), put("acme-paused"));
            // An old record: its engine last wrote it before the backfill.
            let old = put("acme-old");
            std::fs::File::options()
                .write(true)
                .open(old.transcript.as_deref().unwrap())
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(90 * 24 * 3600))
                .unwrap();
            for r in [&bare, &paused, &old] {
                store.put(r).await.unwrap();
            }

            // No redactor: the ledger reads on, the harvest holds.
            let cancel = tokio_util::sync::CancellationToken::new();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                Default::default(),
                cancel.clone(),
            )
            .await
            .unwrap();
            let c = read_through(&d, bare.id).await;
            assert_eq!(c.harvested_to, 0);
            let m = d.session_memory(bare.id).await.unwrap();
            assert_eq!(
                (m.remembering, m.why.as_deref()),
                (false, Some(crate::memory::NO_REDACTOR))
            );
            cancel.cancel();

            // Paused: still held.
            store
                .put_setting(crate::memory_store::PAUSED, &serde_json::json!(true))
                .await
                .unwrap();
            let floor = || super::super::Floor {
                redactor: Some(Arc::new(Plain)),
                ..Default::default()
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                floor(),
                cancel.clone(),
            )
            .await
            .unwrap();
            d.spend_read(paused.id);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(read_through(&d, paused.id).await.harvested_to, 0);
            assert!(d
                .session_memory(paused.id)
                .await
                .unwrap()
                .exchanges
                .is_empty());
            cancel.cancel();

            // Unpaused, with a redactor: what was held is harvested now, and
            // the old record's history never is.
            store
                .put_setting(crate::memory_store::PAUSED, &serde_json::json!(false))
                .await
                .unwrap();
            let d = Desk::start(
                DeskStore::from_pool(pool.clone()),
                crate::testing::options(dir.path()),
                floor(),
                Default::default(),
            )
            .await
            .unwrap();
            for _ in 0..400 {
                if d.session_memory(paused.id).await.unwrap().counts.exchanges == 5 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let m = d.session_memory(paused.id).await.unwrap();
            assert_eq!(m.counts.exchanges, 5, "nothing said while paused was lost");
            assert!(m.remembering);
            let c = read_through(&d, old.id).await;
            for _ in 0..400 {
                if cursor_of(&d, old.id)
                    .await
                    .is_some_and(|c| c.harvested_to > 0)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert_eq!(cursor_of(&d, old.id).await.unwrap().harvested_to, c.read_to);
            assert_eq!(
                d.session_memory(old.id).await.unwrap().counts.exchanges,
                0,
                "older than backfill_days"
            );
        })
        .await;
    }
}
