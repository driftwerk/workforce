//! The one query path of session memory (`Desk::memory_recall`): what the reader
//! may search, a hybrid search of each of its collections, the rows read
//! back from Postgres and checked once more, ranked, cut to the limit, and
//! the read kept. Without an index ready, Postgres's keyword search stands
//! in under the same bound, and the answer says so.

use std::collections::HashMap;

use chrono::Utc;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::memory::index::IndexQuery;
use crate::memory::passage::PassageKind;
use crate::memory::rank::{rank, Candidate, HitKind};
use crate::memory::reach::{bound, may_see, one_group, Bound, Reach, Reader, Row};
use crate::memory_index_store::RecallRow;

/// What is asked of session memory.
#[derive(Debug, Clone, Default)]
pub struct Recall {
    pub q: String,
    /// How far a session asked to look; it can only narrow.
    pub reach: Option<Reach>,
    /// Only one group's (`Some(None)`: ungrouped); it can only narrow.
    pub group: Option<Option<String>>,
    /// Only one session's; it can only narrow.
    pub session: Option<Uuid>,
    /// Only these kinds of exchanges and notes; all when none.
    pub kinds: Option<Vec<PassageKind>>,
    pub limit: usize,
    /// What `memory_reads.tool` says asked.
    pub tool: &'static str,
}

/// What a recall found, best first.
#[derive(Debug, Clone)]
pub struct Recalled {
    pub hits: Vec<(RecallRow, f32)>,
    /// Only keywords were matched: there is no search index ready.
    pub keyword_only: bool,
    pub notice: Option<&'static str>,
    /// The most a session's recall covers, as this one read it.
    pub session_reach: Reach,
}

/// The setting key of the person's narrower session reach.
pub const SESSION_REACH: &str = "memory.session_reach";

/// Relevance given a hit found by its words in Postgres, beside the index's:
/// as the index's own words-only hits are weighed.
const WORDS_ONLY: f32 = 0.5;

impl Desk {
    /// The hits `reader` may read for `r`.
    pub async fn memory_recall(&self, reader: &Reader, r: &Recall) -> Result<Recalled, DeskError> {
        if r.q.trim().is_empty() {
            return Err(DeskError::Invalid("Say what to search for.".into()));
        }
        let limit = r.limit.max(1);
        let known = self.0.store.memory_groups().await?;
        let session_reach = self.session_reach().await;
        let b = bound(reader, session_reach, r.reach, &known)
            .narrowed(r.group.as_ref().map(|g| g.as_deref()), r.session);
        if b.is_empty() {
            tracing::info!(
                reader = %reader.label(),
                tool = r.tool,
                "a memory search asked for nothing its reader may read; nothing was searched"
            );
            return Ok(Recalled {
                hits: Vec::new(),
                keyword_only: self.memory_index_up().is_none(),
                notice: None,
                session_reach,
            });
        }
        let wanted = limit * 3;
        let kinds = r.kinds.as_deref();
        let (found, keyword_only, notice) = match self.indexed(&b, &r.q, kinds, wanted).await {
            Some(mut found) => {
                // What the index doesn't hold yet is found by its words.
                let catching_up = self.0.store.index_backlog(Some(&b.groups)).await? > 0;
                let unindexed = self
                    .0
                    .store
                    .memory_keyword(&r.q, &b, kinds, true, wanted as i64)
                    .await?;
                for (id, words) in unindexed {
                    let e = found.entry(id).or_default();
                    *e = e.max(words * WORDS_ONLY);
                }
                (
                    found,
                    false,
                    catching_up.then_some(crate::memory::CATCHING_UP),
                )
            }
            None => {
                let found = self
                    .0
                    .store
                    .memory_keyword(&r.q, &b, kinds, false, wanted as i64)
                    .await?
                    .into_iter()
                    .collect();
                (found, true, Some(crate::memory::KEYWORD_ONLY))
            }
        };
        let ids: Vec<Uuid> = found.keys().copied().collect();
        // Read back from Postgres: a row gone (a purge not yet run) is
        // dropped, and each is checked against its stored group.
        let candidates: Vec<(RecallRow, Candidate)> = self
            .0
            .store
            .recall_rows(&ids)
            .await?
            .into_iter()
            .filter(|row| {
                may_see(
                    reader,
                    &b,
                    &Row {
                        group: row.group.as_deref(),
                        session: row.session,
                        project: &row.project,
                        at: row.at,
                    },
                )
            })
            .map(|row| {
                let c = Candidate {
                    of: row.id,
                    relevance: found.get(&row.id).copied().unwrap_or(0.0),
                    kind: kind_of(&row),
                    at: row.at,
                    session: row.session,
                    project: row.project.clone(),
                    flagged: row.flagged,
                    superseded: row.item.as_ref().is_some_and(|i| i.superseded),
                };
                (row, c)
            })
            .collect();
        let mut rows: HashMap<Uuid, RecallRow> =
            candidates.iter().map(|(r, _)| (r.id, r.clone())).collect();
        let ranked = rank(
            candidates.into_iter().map(|(_, c)| c).collect(),
            Utc::now(),
            reader,
        );
        let hits: Vec<(RecallRow, f32)> = ranked
            .into_iter()
            .take(limit)
            .filter_map(|(c, s)| Some((rows.remove(&c.of)?, s)))
            .collect();
        let hit_ids: Vec<Uuid> = hits.iter().map(|(r, _)| r.id).collect();
        let group = one_group(hits.iter().map(|(r, _)| r.group.as_deref()));
        self.0
            .store
            .memory_read(
                &reader.label(),
                r.tool,
                Some(&r.q),
                &hit_ids,
                group.as_deref(),
            )
            .await?;
        Ok(Recalled {
            hits,
            keyword_only,
            notice,
            session_reach,
        })
    }

    /// Each exchange the index finds in `b`'s collections, with its
    /// relevance; none while there is no index ready, or when it fails.
    async fn indexed(
        &self,
        b: &Bound,
        q: &str,
        kinds: Option<&[PassageKind]>,
        wanted: usize,
    ) -> Option<HashMap<Uuid, f32>> {
        let index = self.memory_index_up()?;
        let query = IndexQuery {
            text: q.to_string(),
            sessions: b.sessions.clone(),
            projects: b.projects.clone(),
            kinds: kinds.map(<[PassageKind]>::to_vec),
            limit: wanted,
        };
        let mut found = HashMap::new();
        for (collection, group) in b.collections.iter().zip(&b.groups) {
            match index.search(collection, &query).await {
                // The index has no collection where Postgres says it holds
                // this group's rows: it lost it. Searched by keyword, and
                // the group written again.
                Ok(None) => match self.0.store.index_holds_any(group.as_deref()).await {
                    Ok(false) => {}
                    Ok(true) | Err(_) => {
                        let lost = eyre::eyre!("the search index lost the collection {collection}");
                        if let Err(e) = self.0.store.index_requeue_group(group.as_deref()).await {
                            tracing::warn!(
                                error = format!("{e:#}"),
                                collection,
                                "couldn't queue a lost collection to be written again"
                            );
                        }
                        self.memory_index_failed(&lost);
                        return None;
                    }
                },
                Ok(Some(hits)) => {
                    for h in hits {
                        let e = found.entry(h.of).or_insert(h.relevance);
                        *e = e.max(h.relevance);
                    }
                }
                Err(e) => {
                    self.memory_index_failed(&e);
                    return None;
                }
            }
        }
        Some(found)
    }

    /// The most a session's recall covers: the install's, or the person's
    /// narrower setting.
    pub(super) async fn session_reach(&self) -> Reach {
        let install = self.0.opts.memory.session_reach;
        let person = self
            .0
            .store
            .setting(SESSION_REACH)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.as_str().and_then(Reach::parse));
        person.map_or(install, |p| p.min(install))
    }
}

/// How a recalled exchange or note is weighed: a note the person
/// confirmed or wrote above any Desk's model wrote.
fn kind_of(row: &RecallRow) -> HitKind {
    use crate::memory::derive::{ItemKind, ItemState};
    if let Some(item) = &row.item {
        return match (item.confirmed, item.kind, item.state) {
            (true, _, _) => HitKind::Confirmed,
            (false, ItemKind::Decision, _) => HitKind::Decision,
            (false, ItemKind::Todo, ItemState::Open) => HitKind::OpenTodo,
            (false, ItemKind::Todo | ItemKind::Fact, _) => HitKind::Fact,
        };
    }
    if row.compaction {
        HitKind::Compaction
    } else if row.answer.trim().is_empty() && row.stepped {
        HitKind::StepsOnly
    } else {
        HitKind::Exchange
    }
}
