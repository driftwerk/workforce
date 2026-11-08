//! The search index session memory keeps beside Postgres: the hub's vellev
//! node, reached through this trait as the hub installs it (the desk never
//! speaks to vellev itself). Everything in it can be rebuilt from Postgres,
//! which stays the record: text a search finds is read back from there.

use async_trait::async_trait;
use uuid::Uuid;

use super::passage::{Passage, PassageKind, Purge};

/// A search of one collection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexQuery {
    pub text: String,
    /// Only these sessions' passages, when set.
    pub sessions: Option<Vec<Uuid>>,
    /// Only these projects', when set.
    pub projects: Option<Vec<String>>,
    /// Only these kinds, when set.
    pub kinds: Option<Vec<PassageKind>>,
    pub limit: usize,
}

/// A passage a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexHit {
    /// The exchange (or item) it is of.
    pub of: Uuid,
    /// How well it matches, from 0 to 1: the model's certainty on the scale
    /// the hub's recall floors were tuned on where the meaning matched,
    /// else the words' fused score.
    pub relevance: f32,
}

/// How a page of what Postgres says the index holds differs from what it
/// does hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Differing {
    /// Passage keys it lacks, or holds with another text.
    pub keys: Vec<String>,
    /// How many objects it holds under the page's keys.
    pub held: u64,
}

/// One page of a collection's passage keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyPage {
    pub keys: Vec<String>,
    /// Where the next page begins; none after the last.
    pub next: Option<String>,
}

#[async_trait]
pub trait MemoryIndex: Send + Sync {
    /// Makes sure `collection` exists. True when it was made just now, or
    /// made again after it was found gone: it is empty, and everything of
    /// it must be written again.
    async fn ensure(&self, collection: &str) -> eyre::Result<bool>;

    /// Writes `passages` to `collection`, each replacing the one under its
    /// key, and deletes what the index holds of their exchanges beyond
    /// them (an exchange that became shorter).
    async fn write(&self, collection: &str, passages: &[Passage]) -> eyre::Result<()>;

    /// Deletes what `purge` names from `collection`, however many objects
    /// that is; a collection that isn't there has nothing to delete. How
    /// many went.
    async fn purge(&self, collection: &str, purge: &Purge) -> eyre::Result<u64>;

    /// The collections of session memory the index holds.
    async fn collections(&self) -> eyre::Result<Vec<String>>;

    /// How `page`, `(key, hash)` pairs with no key twice, differs from what
    /// `collection` holds.
    async fn differing(
        &self,
        collection: &str,
        page: &[(String, String)],
    ) -> eyre::Result<Differing>;

    /// How many objects `collection` holds.
    async fn count(&self, collection: &str) -> eyre::Result<u64>;

    /// A page of `collection`'s passage keys, after `after`.
    async fn keys(
        &self,
        collection: &str,
        after: Option<String>,
        limit: usize,
    ) -> eyre::Result<KeyPage>;

    /// Writes and deletes a passage of its own, outside every memory
    /// collection: whether the index can embed and write now, which a
    /// listing of its collections doesn't show.
    async fn canary(&self) -> eyre::Result<()>;

    /// The passages of `collection` that best match `q`, by meaning and by
    /// words, best first; one hit per exchange. None when the index has no
    /// such collection: for a group whose rows Postgres says it holds, the
    /// index lost it.
    async fn search(&self, collection: &str, q: &IndexQuery)
        -> eyre::Result<Option<Vec<IndexHit>>>;
}

/// An index in memory, for tests: what the hub's vellev index does, without
/// a node. It can be told to fail.
#[cfg(test)]
pub mod fake {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use parking_lot::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct FakeIndex {
        /// Each collection's passages, by key.
        pub held: Mutex<HashMap<String, BTreeMap<String, Passage>>>,
        /// Every call fails while set.
        pub down: AtomicBool,
        /// How many writes were asked for.
        pub writes: AtomicUsize,
        /// A write holding a passage of one of these fails.
        pub poison: Mutex<HashSet<Uuid>>,
        /// Every deletion fails while set, the rest answering.
        pub purges_fail: AtomicBool,
        /// Reconcile's comparisons fail while set, the rest answering.
        pub differing_fails: AtomicBool,
        /// Every write fails while set, listings answering: an embedder
        /// that is down.
        pub writes_down: AtomicBool,
    }

    impl FakeIndex {
        fn up(&self) -> eyre::Result<()> {
            match self.down.load(Ordering::SeqCst) {
                true => Err(eyre::eyre!("the index isn't answering")),
                false => Ok(()),
            }
        }

        /// Every ref `collection` holds a passage of.
        pub fn refs(&self, collection: &str) -> Vec<Uuid> {
            let mut refs: Vec<Uuid> = self
                .held
                .lock()
                .get(collection)
                .map(|c| c.values().map(|p| p.of).collect())
                .unwrap_or_default();
            refs.sort();
            refs.dedup();
            refs
        }

        /// Every passage held anywhere.
        pub fn all(&self) -> Vec<Passage> {
            self.held
                .lock()
                .values()
                .flat_map(|c| c.values().cloned())
                .collect()
        }
    }

    #[async_trait]
    impl MemoryIndex for FakeIndex {
        async fn ensure(&self, collection: &str) -> eyre::Result<bool> {
            self.up()?;
            let mut held = self.held.lock();
            let made = !held.contains_key(collection);
            held.entry(collection.to_string()).or_default();
            Ok(made)
        }

        async fn write(&self, collection: &str, passages: &[Passage]) -> eyre::Result<()> {
            self.up()?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.writes_down.load(Ordering::SeqCst) {
                eyre::bail!("the index can't embed");
            }
            if passages.iter().any(|p| self.poison.lock().contains(&p.of)) {
                eyre::bail!("the index refused a passage");
            }
            let mut held = self.held.lock();
            let c = held.entry(collection.to_string()).or_default();
            let mut counts: HashMap<Uuid, u32> = HashMap::new();
            for p in passages {
                let n = counts.entry(p.of).or_default();
                *n = (*n).max(p.n + 1);
                c.insert(p.key.clone(), p.clone());
            }
            c.retain(|_, p| counts.get(&p.of).is_none_or(|n| p.n < *n));
            Ok(())
        }

        async fn purge(&self, collection: &str, purge: &Purge) -> eyre::Result<u64> {
            self.up()?;
            if self.purges_fail.load(Ordering::SeqCst) {
                eyre::bail!("the index refused a deletion");
            }
            let mut held = self.held.lock();
            if let Purge::All = purge {
                return Ok(held.remove(collection).map(|c| c.len() as u64).unwrap_or(0));
            }
            let Some(c) = held.get_mut(collection) else {
                return Ok(0);
            };
            let before = c.len();
            c.retain(|_, p| match purge {
                Purge::Session { session } => p.session != *session,
                Purge::Refs { refs } => !refs.contains(&p.of),
                Purge::All => false,
            });
            Ok((before - c.len()) as u64)
        }

        async fn canary(&self) -> eyre::Result<()> {
            self.up()?;
            match self.writes_down.load(Ordering::SeqCst) {
                true => eyre::bail!("the index can't embed"),
                false => Ok(()),
            }
        }

        async fn collections(&self) -> eyre::Result<Vec<String>> {
            self.up()?;
            Ok(self.held.lock().keys().cloned().collect())
        }

        async fn differing(
            &self,
            collection: &str,
            page: &[(String, String)],
        ) -> eyre::Result<Differing> {
            self.up()?;
            if self.differing_fails.load(Ordering::SeqCst) {
                eyre::bail!("the index couldn't compare");
            }
            let held = self.held.lock();
            let c = held.get(collection);
            let mut out = Differing::default();
            for (key, hash) in page {
                match c.and_then(|c| c.get(key)) {
                    Some(p) => {
                        out.held += 1;
                        if p.hash != *hash {
                            out.keys.push(key.clone());
                        }
                    }
                    None => out.keys.push(key.clone()),
                }
            }
            Ok(out)
        }

        async fn count(&self, collection: &str) -> eyre::Result<u64> {
            self.up()?;
            Ok(self
                .held
                .lock()
                .get(collection)
                .map(|c| c.len() as u64)
                .unwrap_or(0))
        }

        async fn keys(
            &self,
            collection: &str,
            after: Option<String>,
            limit: usize,
        ) -> eyre::Result<KeyPage> {
            self.up()?;
            let held = self.held.lock();
            let Some(c) = held.get(collection) else {
                return Ok(KeyPage::default());
            };
            let keys: Vec<String> = c
                .keys()
                .filter(|k| after.as_ref().is_none_or(|a| *k > a))
                .take(limit + 1)
                .cloned()
                .collect();
            let more = keys.len() > limit;
            let keys: Vec<String> = keys.into_iter().take(limit).collect();
            Ok(KeyPage {
                next: more.then(|| keys.last().cloned()).flatten(),
                keys,
            })
        }

        /// By words alone: a passage matches by how many of the query's
        /// words it holds.
        async fn search(
            &self,
            collection: &str,
            q: &IndexQuery,
        ) -> eyre::Result<Option<Vec<IndexHit>>> {
            self.up()?;
            let words: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
            let held = self.held.lock();
            let Some(c) = held.get(collection) else {
                return Ok(None);
            };
            let mut best: HashMap<Uuid, f32> = HashMap::new();
            for p in c.values() {
                if q.sessions.as_ref().is_some_and(|s| !s.contains(&p.session))
                    || q.projects.as_ref().is_some_and(|s| !s.contains(&p.project))
                    || q.kinds.as_ref().is_some_and(|k| !k.contains(&p.kind))
                {
                    continue;
                }
                let text = p.text.to_lowercase();
                let found = words.iter().filter(|w| text.contains(w.as_str())).count();
                if found == 0 {
                    continue;
                }
                let score = found as f32 / words.len().max(1) as f32;
                let e = best.entry(p.of).or_default();
                *e = e.max(score);
            }
            let mut hits: Vec<IndexHit> = best
                .into_iter()
                .map(|(of, relevance)| IndexHit { of, relevance })
                .collect();
            hits.sort_by(|a, b| b.relevance.total_cmp(&a.relevance).then(a.of.cmp(&b.of)));
            hits.truncate(q.limit);
            Ok(Some(hits))
        }
    }
}
