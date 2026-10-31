//! Where know-how lives: entries, who produced them (as per-agent hashes) and
//! the intents that match requests to them. Postgres when the install has it
//! (shared by every replica); in memory otherwise and in tests.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use parking_lot::Mutex;

/// Intents kept per entry: enough to match differently worded requests,
/// few enough that matching stays cheap.
pub const INTENTS_PER_ENTRY: i64 = 12;

pub use workforce_common::settings::KnowHowKind as Kind;

/// Data kinds name a Metabase site and database; code locations leave them
/// empty (`""` and `0`) and name a repo instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub kind: Kind,
    pub key: String,
    /// The Metabase site (normalised) the tables belong to.
    pub site: String,
    pub database_id: i64,
    /// For a correction: the real table it points to.
    pub tables: Vec<String>,
    /// For a column correction: the real column it points to.
    pub columns: Vec<String>,
    pub shape: Option<String>,
    /// For a correction: the name that does not exist (`table.column`, or a
    /// table).
    pub wrong: Option<String>,
    /// For a code location: the repo (`owner/repo`), the repo-relative path
    /// and the symbol the search hit, if it hit one.
    pub repo: Option<String>,
    pub path: Option<String>,
    pub symbol: Option<String>,
    /// An admin's description, on entries an admin added.
    pub note: Option<String>,
    /// Added by hand rather than learned from a turn.
    pub manual: bool,
    pub pinned: bool,
    /// A re-check could not find one of its names; not shown until a turn or
    /// a later re-check finds it again.
    pub stale: bool,
    /// Different people who produced it.
    pub people: u32,
    pub uses: u32,
    pub first_seen: DateTime<Utc>,
    /// Last produced, shown or matched.
    pub last_seen: DateTime<Utc>,
}

/// One person producing an entry: from a finished turn, or added by hand.
#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: Kind,
    pub key: String,
    pub site: String,
    pub database_id: i64,
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    pub shape: Option<String>,
    pub wrong: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub symbol: Option<String>,
    pub note: Option<String>,
    /// [`super::KnowHow::person`] of whoever produced it.
    pub person: String,
    pub intent: Option<IntentVector>,
    pub manual: bool,
    pub pin: bool,
}

impl Observation {
    /// `person` producing the entry `key`, every name still to be filled in.
    pub fn of(kind: Kind, key: String, person: String) -> Self {
        Self {
            kind,
            key,
            site: String::new(),
            database_id: 0,
            tables: Vec::new(),
            columns: Vec::new(),
            shape: None,
            wrong: None,
            repo: None,
            path: None,
            symbol: None,
            note: None,
            person,
            intent: None,
            manual: false,
            pin: false,
        }
    }
}

/// What a re-check found for one entry. It is applied to the entry as it
/// is when written, not as it was read, so a turn recording at the same
/// time loses nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Revision {
    pub id: i64,
    /// Columns the schema no longer has.
    pub gone: Vec<String>,
    /// For a symbol now defined in another file: the entry's key and path
    /// there. Taken already, by any entry, the entry goes stale instead.
    pub moved: Option<(String, String)>,
    /// One of its names is gone, unless a turn produced it since the
    /// re-check read it; `false` brings a stale entry back.
    pub stale: bool,
}

/// An intent's embedding; its text is not kept.
#[derive(Debug, Clone)]
pub struct IntentVector {
    pub model: String,
    pub vector: Vec<f32>,
}

#[async_trait]
pub trait KnowHowStore: Send + Sync {
    /// Upsert the entry, count the person once, keep their one intent (and
    /// the newest [`INTENTS_PER_ENTRY`] overall); returns the entry id. A
    /// produced entry is no longer stale. A removed entry stays removed and
    /// records nothing more.
    async fn record(&self, agent: &str, obs: Observation) -> Result<i64>;
    /// Every live (not removed) entry of the agent, stale ones included.
    async fn entries(&self, agent: &str) -> Result<Vec<Entry>>;
    /// `(entry id, vector)` for every intent embedded with `model`.
    async fn intents(&self, agent: &str, model: &str) -> Result<Vec<(i64, Vec<f32>)>>;
    /// Entries `person` produced.
    async fn produced_by(&self, agent: &str, person: &str) -> Result<HashSet<i64>>;
    async fn set_pinned(&self, agent: &str, id: i64, pinned: bool) -> Result<bool>;
    /// Remove an entry for good: it is never learned again.
    async fn retire(&self, agent: &str, id: i64) -> Result<bool>;
    /// Delete an entry and everything recorded for it; later turns may learn
    /// it afresh.
    async fn delete(&self, agent: &str, id: i64) -> Result<bool>;
    /// Apply what a re-check that read the entries at `checked` found.
    /// `false` when the entry is gone or removed.
    async fn revise(&self, agent: &str, r: &Revision, checked: DateTime<Utc>) -> Result<bool>;
    /// Delete every entry not produced, shown or matched since `before`,
    /// except pinned and removed ones. Returns how many went.
    async fn prune_unused(&self, before: DateTime<Utc>) -> Result<usize>;
    /// Remove everything `person` contributed: their provenance, intents and
    /// column reads, and the columns only they read from what the entries
    /// show. Entries nobody else produced go, unless an admin pinned them
    /// as additions by hand. Returns the entries touched.
    async fn forget_person(&self, agent: &str, person: &str) -> Result<usize>;
    /// Make everything `from` contributed `to`'s, as one person: where both
    /// produced an entry or read a column it counts once, and `to`'s own
    /// intent for an entry is the one kept. Returns the entries touched.
    async fn reassign_person(&self, agent: &str, from: &str, to: &str) -> Result<usize>;
    async fn event(
        &self,
        agent: &str,
        entry: Option<i64>,
        kind: &str,
        score: Option<f32>,
    ) -> Result<()>;
    /// The entries were matched for a request, and `shown` with it: both
    /// count as use.
    async fn mark_used(&self, agent: &str, ids: &[i64], shown: bool) -> Result<()>;
    /// Agents with any know-how.
    async fn agents(&self) -> Result<Vec<String>>;
    /// How many different people read each column (`table.column`) of each
    /// site and database.
    async fn column_people(&self, agent: &str) -> Result<HashMap<ColumnKey, u32>>;
}

/// `(site, database id, "table.column")`.
pub type ColumnKey = (String, i64, String);

/// What every backend's `reassign_person` must do alike.
#[cfg(test)]
pub(crate) async fn reassign_person_contract(s: &dyn KnowHowStore) {
    let obs = |person: &str, x: f32| Observation {
        site: "mb".into(),
        database_id: 3,
        tables: vec!["public.orders".into()],
        columns: vec!["public.orders.status".into()],
        intent: Some(IntentVector {
            model: "m".into(),
            vector: vec![x, 1.0],
        }),
        ..Observation::of(
            Kind::DataLocation,
            "data:mb:3:public.orders".into(),
            person.into(),
        )
    };
    let id = s.record("a", obs("p1", 0.1)).await.unwrap();
    s.record("a", obs("p2", 0.2)).await.unwrap();
    s.record("a", obs("p3", 0.3)).await.unwrap();
    s.record("b", obs("p1", 0.4)).await.unwrap();

    assert_eq!(s.reassign_person("a", "p1", "p2").await.unwrap(), 1);
    assert_eq!(
        s.entries("a").await.unwrap()[0].people,
        2,
        "one person, counted once"
    );
    assert!(s.produced_by("a", "p1").await.unwrap().is_empty());
    assert!(s.produced_by("a", "p2").await.unwrap().contains(&id));
    assert_eq!(
        s.intents("a", "m").await.unwrap().len(),
        2,
        "p2's own intent is kept"
    );
    assert_eq!(
        s.column_people("a").await.unwrap()
            [&("mb".to_string(), 3, "public.orders.status".to_string())],
        2
    );
    assert!(
        !s.produced_by("b", "p1").await.unwrap().is_empty(),
        "other agents untouched"
    );
    assert_eq!(s.reassign_person("a", "p1", "p2").await.unwrap(), 0);
}

#[derive(Default)]
pub struct InMemoryKnowHowStore {
    inner: Mutex<Mem>,
}

#[derive(Default)]
struct Mem {
    next: i64,
    entries: HashMap<(String, String), Entry>,
    retired: HashSet<i64>,
    agent_of: HashMap<i64, String>,
    provenance: HashMap<i64, HashSet<String>>,
    intents: HashMap<i64, Vec<(String, String, Vec<f32>)>>,
    columns: HashMap<(String, ColumnKey), HashSet<String>>,
    /// When a turn last produced each entry.
    produced: HashMap<i64, DateTime<Utc>>,
    events: Vec<(String, Option<i64>, String)>,
}

impl Mem {
    fn key_of(&self, agent: &str, id: i64) -> Option<(String, String)> {
        self.entries
            .iter()
            .find(|((a, _), e)| a == agent && e.id == id)
            .map(|(k, _)| k.clone())
    }

    fn drop_entry(&mut self, key: &(String, String)) {
        if let Some(e) = self.entries.remove(key) {
            self.intents.remove(&e.id);
            self.provenance.remove(&e.id);
            self.produced.remove(&e.id);
            self.agent_of.remove(&e.id);
        }
    }

    /// Forget who read a column once no entry names it.
    fn drop_orphan_columns(&mut self) {
        let named: HashSet<(&str, &str, i64, &str)> = self
            .entries
            .iter()
            .flat_map(|((a, _), e)| {
                e.columns
                    .iter()
                    .map(move |c| (a.as_str(), e.site.as_str(), e.database_id, c.as_str()))
            })
            .collect();
        let orphans: Vec<(String, ColumnKey)> = self
            .columns
            .keys()
            .filter(|(a, (site, db, c))| {
                !named.contains(&(a.as_str(), site.as_str(), *db, c.as_str()))
            })
            .cloned()
            .collect();
        for k in orphans {
            self.columns.remove(&k);
        }
    }
}

impl InMemoryKnowHowStore {
    pub fn events(&self) -> Vec<(Option<i64>, String)> {
        self.inner
            .lock()
            .events
            .iter()
            .map(|(_, e, k)| (*e, k.clone()))
            .collect()
    }

    /// Make an entry look last seen at `at` (tests of ageing).
    #[cfg(test)]
    pub fn set_last_seen(&self, id: i64, at: DateTime<Utc>) {
        let mut m = self.inner.lock();
        if let Some(e) = m.entries.values_mut().find(|e| e.id == id) {
            e.last_seen = at;
        }
    }
}

#[async_trait]
impl KnowHowStore for InMemoryKnowHowStore {
    async fn record(&self, agent: &str, obs: Observation) -> Result<i64> {
        let mut m = self.inner.lock();
        let k = (agent.to_string(), obs.key.clone());
        let now = Utc::now();
        let id = match m.entries.get(&k) {
            Some(e) => e.id,
            None => {
                m.next += 1;
                let id = m.next;
                m.entries.insert(
                    k.clone(),
                    Entry {
                        id,
                        kind: obs.kind,
                        key: obs.key.clone(),
                        site: obs.site.clone(),
                        database_id: obs.database_id,
                        tables: obs.tables.clone(),
                        columns: obs.columns.clone(),
                        shape: obs.shape.clone(),
                        wrong: obs.wrong.clone(),
                        repo: obs.repo.clone(),
                        path: obs.path.clone(),
                        symbol: obs.symbol.clone(),
                        note: None,
                        manual: obs.manual,
                        pinned: false,
                        stale: false,
                        people: 0,
                        uses: 0,
                        first_seen: now,
                        last_seen: now,
                    },
                );
                m.agent_of.insert(id, agent.to_string());
                id
            }
        };
        if m.retired.contains(&id) {
            return Ok(id);
        }
        for c in &obs.columns {
            m.columns
                .entry((
                    agent.to_string(),
                    (obs.site.clone(), obs.database_id, c.clone()),
                ))
                .or_default()
                .insert(obs.person.clone());
        }
        let people = {
            let p = m.provenance.entry(id).or_default();
            p.insert(obs.person.clone());
            p.len() as u32
        };
        m.produced.insert(id, now);
        if let Some(intent) = obs.intent {
            let list = m.intents.entry(id).or_default();
            list.retain(|(p, _, _)| *p != obs.person);
            list.push((obs.person.clone(), intent.model, intent.vector));
            let excess = list.len().saturating_sub(INTENTS_PER_ENTRY as usize);
            list.drain(..excess);
        }
        let e = m.entries.get_mut(&k).expect("inserted above");
        for c in &obs.columns {
            if !e.columns.contains(c) {
                e.columns.push(c.clone());
            }
        }
        e.columns.sort();
        e.people = people;
        e.last_seen = now;
        e.stale = false;
        if obs.pin {
            e.pinned = true;
        }
        if obs.note.is_some() {
            e.note = obs.note;
        }
        Ok(id)
    }

    async fn entries(&self, agent: &str) -> Result<Vec<Entry>> {
        let m = self.inner.lock();
        let mut out: Vec<Entry> = m
            .entries
            .iter()
            .filter(|((a, _), e)| a == agent && !m.retired.contains(&e.id))
            .map(|(_, e)| e.clone())
            .collect();
        out.sort_by_key(|e| e.id);
        Ok(out)
    }

    async fn intents(&self, agent: &str, model: &str) -> Result<Vec<(i64, Vec<f32>)>> {
        let m = self.inner.lock();
        Ok(m.intents
            .iter()
            .filter(|(id, _)| {
                m.agent_of.get(id).is_some_and(|a| a == agent) && !m.retired.contains(id)
            })
            .flat_map(|(id, list)| {
                list.iter()
                    .filter(|(_, mdl, _)| mdl == model)
                    .map(move |(_, _, v)| (*id, v.clone()))
            })
            .collect())
    }

    async fn produced_by(&self, agent: &str, person: &str) -> Result<HashSet<i64>> {
        let m = self.inner.lock();
        Ok(m.provenance
            .iter()
            .filter(|(id, p)| m.agent_of.get(id).is_some_and(|a| a == agent) && p.contains(person))
            .map(|(id, _)| *id)
            .collect())
    }

    async fn set_pinned(&self, agent: &str, id: i64, pinned: bool) -> Result<bool> {
        let mut m = self.inner.lock();
        if m.retired.contains(&id) {
            return Ok(false);
        }
        let Some(key) = m.key_of(agent, id) else {
            return Ok(false);
        };
        if let Some(e) = m.entries.get_mut(&key) {
            e.pinned = pinned;
        }
        Ok(true)
    }

    async fn retire(&self, agent: &str, id: i64) -> Result<bool> {
        let mut m = self.inner.lock();
        if m.agent_of.get(&id).is_some_and(|a| a == agent) {
            m.retired.insert(id);
            return Ok(true);
        }
        Ok(false)
    }

    async fn delete(&self, agent: &str, id: i64) -> Result<bool> {
        let mut m = self.inner.lock();
        let Some(key) = m.key_of(agent, id) else {
            return Ok(false);
        };
        m.drop_entry(&key);
        m.retired.remove(&id);
        m.drop_orphan_columns();
        Ok(true)
    }

    async fn revise(&self, agent: &str, r: &Revision, checked: DateTime<Utc>) -> Result<bool> {
        let mut m = self.inner.lock();
        if m.retired.contains(&r.id) {
            return Ok(false);
        }
        let Some(key) = m.key_of(agent, r.id) else {
            return Ok(false);
        };
        let fresh = m.produced.get(&r.id).is_some_and(|at| *at > checked);
        let moved = r.moved.as_ref().filter(|_| !fresh);
        let taken = moved.is_some_and(|(k, _)| {
            m.entries
                .iter()
                .any(|((a, ek), e)| a == agent && ek == k && e.id != r.id)
        });
        let mut e = m.entries.remove(&key).expect("found above");
        e.columns.retain(|c| !r.gone.contains(c));
        e.stale = (r.stale || taken) && !fresh;
        let key = match moved {
            Some((k, path)) if !taken => {
                e.key = k.clone();
                e.path = Some(path.clone());
                (agent.to_string(), k.clone())
            }
            _ => key,
        };
        m.entries.insert(key, e);
        Ok(true)
    }

    async fn prune_unused(&self, before: DateTime<Utc>) -> Result<usize> {
        let mut m = self.inner.lock();
        let old: Vec<(String, String)> = m
            .entries
            .iter()
            .filter(|(_, e)| !e.pinned && !m.retired.contains(&e.id) && e.last_seen < before)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &old {
            m.drop_entry(k);
        }
        m.drop_orphan_columns();
        Ok(old.len())
    }

    async fn forget_person(&self, agent: &str, person: &str) -> Result<usize> {
        let mut m = self.inner.lock();
        for ((a, _), people) in m.columns.iter_mut() {
            if a == agent {
                people.remove(person);
            }
        }
        let ids: Vec<i64> = m
            .provenance
            .iter()
            .filter(|(id, p)| m.agent_of.get(id).is_some_and(|a| a == agent) && p.contains(person))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(list) = m.intents.get_mut(id) {
                list.retain(|(p, _, _)| p != person);
            }
            let left = {
                let p = m.provenance.get_mut(id).expect("listed above");
                p.remove(person);
                p.len() as u32
            };
            if let Some(key) = m.key_of(agent, *id) {
                let e = &m.entries[&key];
                if left == 0 && !(e.pinned && e.manual) {
                    m.drop_entry(&key);
                    continue;
                }
                let read: HashSet<String> = m
                    .columns
                    .iter()
                    .filter(|((a, (site, db, _)), p)| {
                        a == agent && *site == e.site && *db == e.database_id && !p.is_empty()
                    })
                    .map(|((_, (_, _, c)), _)| c.clone())
                    .collect();
                if let Some(e) = m.entries.get_mut(&key) {
                    e.people = left;
                    if e.kind == Kind::DataLocation && !e.manual {
                        e.columns.retain(|c| read.contains(c));
                    }
                }
            }
        }
        m.drop_orphan_columns();
        Ok(ids.len())
    }

    async fn reassign_person(&self, agent: &str, from: &str, to: &str) -> Result<usize> {
        let mut m = self.inner.lock();
        for ((a, _), people) in m.columns.iter_mut() {
            if a == agent && people.remove(from) {
                people.insert(to.to_string());
            }
        }
        let ids: Vec<i64> = m
            .provenance
            .iter()
            .filter(|(id, p)| m.agent_of.get(id).is_some_and(|a| a == agent) && p.contains(from))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(list) = m.intents.get_mut(id) {
                let theirs = list.iter().any(|(p, _, _)| p == to);
                list.retain(|(p, _, _)| !(theirs && p == from));
                list.iter_mut()
                    .filter(|(p, _, _)| p == from)
                    .for_each(|(p, _, _)| *p = to.to_string());
            }
            let left = {
                let p = m.provenance.get_mut(id).expect("listed above");
                p.remove(from);
                p.insert(to.to_string());
                p.len() as u32
            };
            if let Some(key) = m.key_of(agent, *id) {
                if let Some(e) = m.entries.get_mut(&key) {
                    e.people = left;
                }
            }
        }
        Ok(ids.len())
    }

    async fn event(
        &self,
        agent: &str,
        entry: Option<i64>,
        kind: &str,
        _score: Option<f32>,
    ) -> Result<()> {
        self.inner
            .lock()
            .events
            .push((agent.to_string(), entry, kind.to_string()));
        Ok(())
    }

    async fn column_people(&self, agent: &str) -> Result<HashMap<ColumnKey, u32>> {
        let m = self.inner.lock();
        Ok(m.columns
            .iter()
            .filter(|((a, _), p)| a == agent && !p.is_empty())
            .map(|((_, k), p)| (k.clone(), p.len() as u32))
            .collect())
    }

    async fn agents(&self) -> Result<Vec<String>> {
        let m = self.inner.lock();
        let mut out: Vec<String> = m
            .agent_of
            .values()
            .chain(m.columns.keys().map(|(a, _)| a))
            .cloned()
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn mark_used(&self, agent: &str, ids: &[i64], shown: bool) -> Result<()> {
        let mut m = self.inner.lock();
        let now = Utc::now();
        for ((a, _), e) in m.entries.iter_mut() {
            if a == agent && ids.contains(&e.id) {
                e.uses += u32::from(shown);
                e.last_seen = now;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "postgres")]
pub use pg::PostgresKnowHowStore;

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use eyre::Context;
    use workforce_postgres::{
        MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
    };

    // Own directory, never a subdir of another embed root — refinery's
    // embed_migrations is recursive.
    mod embedded {
        refinery::embed_migrations!("migrations_know_how");
    }

    struct Migrations;

    #[async_trait]
    impl MigrationRun for Migrations {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            embedded::migrations::runner()
                .run_async(client)
                .await
                .map_err(PostgresError::from)?;
            Ok(())
        }
    }

    pub struct PostgresKnowHowStore {
        pool: PostgresPool,
    }

    /// Forget who read a column of the agents `$1` once no entry names it.
    const ORPHAN_COLUMNS: &str =
        "DELETE FROM column_people cp WHERE cp.agent_id = ANY($1) AND NOT EXISTS (
        SELECT 1 FROM entries e WHERE e.agent_id = cp.agent_id AND e.site = cp.site
            AND e.database_id = cp.database_id AND cp.name = ANY(e.columns))";

    impl PostgresKnowHowStore {
        pub const SCHEMA: &'static str = "know_how";

        pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
            PostgresPool::from_config_with_session_init(
                &config.small(),
                &["SET search_path TO know_how, public"],
            )
        }

        pub async fn new(config: &PostgresConfig) -> Result<Self> {
            let pool = Self::build_pool(config).context("building know-how pool")?;
            Self::with_pool(pool).await
        }

        pub async fn with_pool(pool: PostgresPool) -> Result<Self> {
            MigrationRunner::run(
                &pool,
                &[MigrationSet::new(
                    Self::SCHEMA,
                    Box::new(Migrations) as Box<dyn MigrationRun>,
                )],
            )
            .await
            .context("running know-how migrations")?;
            Ok(Self { pool })
        }
    }

    fn entry(r: &tokio_postgres::Row) -> Option<Entry> {
        Some(Entry {
            id: r.get("id"),
            kind: Kind::parse(r.get::<_, &str>("kind"))?,
            key: r.get("key"),
            site: r.get("site"),
            database_id: r.get("database_id"),
            tables: r.get("tables"),
            columns: r.get("columns"),
            shape: r.get("shape"),
            wrong: r.get("wrong"),
            repo: r.get("repo"),
            path: r.get("path"),
            symbol: r.get("symbol"),
            note: r.get("note"),
            manual: r.get("manual"),
            pinned: r.get("pinned"),
            stale: r.get("stale"),
            people: r.get::<_, i32>("people").max(0) as u32,
            uses: r.get::<_, i32>("uses").max(0) as u32,
            first_seen: r.get("first_seen"),
            last_seen: r.get("last_seen"),
        })
    }

    #[async_trait]
    impl KnowHowStore for PostgresKnowHowStore {
        async fn record(&self, agent: &str, obs: Observation) -> Result<i64> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let id = tx
                .query_one(
                    "INSERT INTO entries (agent_id, kind, key, site, database_id, tables, columns, shape, note, manual, pinned,
                                          wrong, repo, path, symbol)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
                     ON CONFLICT (agent_id, key) DO UPDATE SET
                        last_seen = CASE WHEN entries.retired THEN entries.last_seen ELSE now() END,
                        stale = entries.stale AND entries.retired,
                        pinned = entries.pinned OR (EXCLUDED.pinned AND NOT entries.retired),
                        note = CASE WHEN entries.retired THEN entries.note ELSE COALESCE(EXCLUDED.note, entries.note) END,
                        columns = CASE WHEN entries.retired THEN entries.columns ELSE
                            ARRAY(SELECT DISTINCT c FROM unnest(entries.columns || EXCLUDED.columns) AS c ORDER BY c) END
                     RETURNING id, retired",
                    &[
                        &agent,
                        &obs.kind.as_str(),
                        &obs.key,
                        &obs.site,
                        &obs.database_id,
                        &obs.tables,
                        &obs.columns,
                        &obs.shape,
                        &obs.note,
                        &obs.manual,
                        &obs.pin,
                        &obs.wrong,
                        &obs.repo,
                        &obs.path,
                        &obs.symbol,
                    ],
                )
                .await?;
            let (id, retired): (i64, bool) = (id.get(0), id.get(1));
            if retired {
                tx.commit().await?;
                return Ok(id);
            }
            for c in &obs.columns {
                tx.execute(
                    "INSERT INTO column_people (agent_id, site, database_id, name, person) VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (agent_id, site, database_id, name, person) DO UPDATE SET last_seen = now()",
                    &[&agent, &obs.site, &obs.database_id, c, &obs.person],
                )
                .await?;
            }
            tx.execute(
                "INSERT INTO provenance (entry_id, person) VALUES ($1, $2)
                 ON CONFLICT (entry_id, person) DO UPDATE SET last_seen = now()",
                &[&id, &obs.person],
            )
            .await?;
            // Counted from the rows, so replicas recording at once agree.
            tx.execute(
                "UPDATE entries SET people = (SELECT count(*) FROM provenance WHERE entry_id = $1) WHERE id = $1",
                &[&id],
            )
            .await?;
            if let Some(intent) = &obs.intent {
                tx.execute(
                    "DELETE FROM intents WHERE entry_id = $1 AND person = $2",
                    &[&id, &obs.person],
                )
                .await?;
                tx.execute(
                    "INSERT INTO intents (entry_id, person, model, vector) VALUES ($1, $2, $3, $4)",
                    &[&id, &obs.person, &intent.model, &intent.vector],
                )
                .await?;
                tx.execute(
                    "DELETE FROM intents WHERE entry_id = $1 AND id NOT IN
                        (SELECT id FROM intents WHERE entry_id = $1 ORDER BY id DESC LIMIT $2)",
                    &[&id, &INTENTS_PER_ENTRY],
                )
                .await?;
            }
            tx.commit().await?;
            Ok(id)
        }

        async fn entries(&self, agent: &str) -> Result<Vec<Entry>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT * FROM entries WHERE agent_id = $1 AND NOT retired ORDER BY id",
                    &[&agent],
                )
                .await?;
            Ok(rows.iter().filter_map(entry).collect())
        }

        async fn intents(&self, agent: &str, model: &str) -> Result<Vec<(i64, Vec<f32>)>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT i.entry_id, i.vector FROM intents i JOIN entries e ON e.id = i.entry_id
                     WHERE e.agent_id = $1 AND NOT e.retired AND i.model = $2",
                    &[&agent, &model],
                )
                .await?;
            Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
        }

        async fn produced_by(&self, agent: &str, person: &str) -> Result<HashSet<i64>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT p.entry_id FROM provenance p JOIN entries e ON e.id = p.entry_id
                     WHERE e.agent_id = $1 AND p.person = $2",
                    &[&agent, &person],
                )
                .await?;
            Ok(rows.iter().map(|r| r.get(0)).collect())
        }

        async fn set_pinned(&self, agent: &str, id: i64, pinned: bool) -> Result<bool> {
            let conn = self.pool.get().await?;
            Ok(conn
                .execute(
                    "UPDATE entries SET pinned = $3 WHERE agent_id = $1 AND id = $2 AND NOT retired",
                    &[&agent, &id, &pinned],
                )
                .await?
                > 0)
        }

        async fn retire(&self, agent: &str, id: i64) -> Result<bool> {
            let conn = self.pool.get().await?;
            Ok(conn
                .execute(
                    "UPDATE entries SET retired = TRUE WHERE agent_id = $1 AND id = $2",
                    &[&agent, &id],
                )
                .await?
                > 0)
        }

        async fn delete(&self, agent: &str, id: i64) -> Result<bool> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let deleted = tx
                .execute(
                    "DELETE FROM entries WHERE agent_id = $1 AND id = $2",
                    &[&agent, &id],
                )
                .await?;
            tx.execute(ORPHAN_COLUMNS, &[&[agent].as_slice()]).await?;
            tx.commit().await?;
            Ok(deleted > 0)
        }

        async fn revise(&self, agent: &str, r: &Revision, checked: DateTime<Utc>) -> Result<bool> {
            let conn = self.pool.get().await?;
            let (key, path) = r.moved.clone().unzip();
            Ok(conn
                .execute(
                    "WITH cur AS (
                        SELECT e.id,
                            EXISTS (SELECT 1 FROM provenance p WHERE p.entry_id = e.id AND p.last_seen > $6) AS fresh,
                            EXISTS (SELECT 1 FROM entries o WHERE o.agent_id = e.agent_id AND o.key = $4 AND o.id <> e.id) AS taken
                        FROM entries e WHERE e.agent_id = $1 AND e.id = $2 AND NOT e.retired)
                     UPDATE entries e SET
                        columns = ARRAY(SELECT c FROM unnest(e.columns) AS c WHERE c <> ALL($3) ORDER BY c),
                        key = CASE WHEN $4::TEXT IS NULL OR cur.fresh OR cur.taken THEN e.key ELSE $4 END,
                        path = CASE WHEN $4::TEXT IS NULL OR cur.fresh OR cur.taken THEN e.path ELSE $5 END,
                        stale = ($7 OR ($4::TEXT IS NOT NULL AND cur.taken)) AND NOT cur.fresh
                     FROM cur WHERE e.id = cur.id",
                    &[&agent, &r.id, &r.gone, &key, &path, &checked, &r.stale],
                )
                .await?
                > 0)
        }

        async fn prune_unused(&self, before: DateTime<Utc>) -> Result<usize> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let agents: Vec<String> = tx
                .query(
                    "DELETE FROM entries WHERE NOT pinned AND NOT retired AND last_seen < $1 RETURNING agent_id",
                    &[&before],
                )
                .await?
                .iter()
                .map(|r| r.get(0))
                .collect();
            tx.execute(ORPHAN_COLUMNS, &[&agents]).await?;
            tx.commit().await?;
            Ok(agents.len())
        }

        async fn reassign_person(&self, agent: &str, from: &str, to: &str) -> Result<usize> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let ids: Vec<i64> = tx
                .query(
                    "SELECT p.entry_id FROM provenance p JOIN entries e ON e.id = p.entry_id
                     WHERE e.agent_id = $1 AND p.person = $2",
                    &[&agent, &from],
                )
                .await?
                .iter()
                .map(|r| r.get(0))
                .collect();
            for sql in [
                "DELETE FROM column_people c WHERE c.agent_id = $1 AND c.person = $2
                   AND EXISTS (SELECT 1 FROM column_people o
                               WHERE o.agent_id = c.agent_id AND o.site = c.site
                                 AND o.database_id = c.database_id AND o.name = c.name
                                 AND o.person = $3)",
                "UPDATE column_people SET person = $3 WHERE agent_id = $1 AND person = $2",
                "DELETE FROM provenance p USING entries e
                  WHERE e.id = p.entry_id AND e.agent_id = $1 AND p.person = $2
                    AND EXISTS (SELECT 1 FROM provenance o
                                WHERE o.entry_id = p.entry_id AND o.person = $3)",
                "UPDATE provenance p SET person = $3 FROM entries e
                  WHERE e.id = p.entry_id AND e.agent_id = $1 AND p.person = $2",
                "DELETE FROM intents i USING entries e
                  WHERE e.id = i.entry_id AND e.agent_id = $1 AND i.person = $2
                    AND EXISTS (SELECT 1 FROM intents o
                                WHERE o.entry_id = i.entry_id AND o.person = $3)",
                "UPDATE intents i SET person = $3 FROM entries e
                  WHERE e.id = i.entry_id AND e.agent_id = $1 AND i.person = $2",
            ] {
                tx.execute(sql, &[&agent, &from, &to]).await?;
            }
            tx.execute(
                "UPDATE entries e SET people = (SELECT COUNT(*) FROM provenance p
                                                WHERE p.entry_id = e.id)
                 WHERE e.agent_id = $1 AND e.id = ANY($2)",
                &[&agent, &ids],
            )
            .await?;
            tx.commit().await?;
            Ok(ids.len())
        }

        async fn forget_person(&self, agent: &str, person: &str) -> Result<usize> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let ids: Vec<i64> = tx
                .query(
                    "SELECT p.entry_id FROM provenance p JOIN entries e ON e.id = p.entry_id
                     WHERE e.agent_id = $1 AND p.person = $2",
                    &[&agent, &person],
                )
                .await?
                .iter()
                .map(|r| r.get(0))
                .collect();
            tx.execute(
                "DELETE FROM column_people WHERE agent_id = $1 AND person = $2",
                &[&agent, &person],
            )
            .await?;
            tx.execute(
                "DELETE FROM intents WHERE person = $2 AND entry_id = ANY($1)",
                &[&ids, &person],
            )
            .await?;
            tx.execute(
                "DELETE FROM provenance WHERE person = $2 AND entry_id = ANY($1)",
                &[&ids, &person],
            )
            .await?;
            tx.execute(
                "UPDATE entries SET people = (SELECT count(*) FROM provenance WHERE entry_id = entries.id)
                 WHERE id = ANY($1)",
                &[&ids],
            )
            .await?;
            tx.execute(
                "UPDATE entries e SET columns = ARRAY(SELECT c FROM unnest(e.columns) AS c WHERE EXISTS (
                    SELECT 1 FROM column_people cp WHERE cp.agent_id = e.agent_id AND cp.site = e.site
                        AND cp.database_id = e.database_id AND cp.name = c) ORDER BY c)
                 WHERE e.id = ANY($1) AND e.kind = $2 AND NOT e.manual",
                &[&ids, &Kind::DataLocation.as_str()],
            )
            .await?;
            tx.execute(
                "DELETE FROM entries WHERE id = ANY($1) AND people = 0 AND NOT (pinned AND manual)",
                &[&ids],
            )
            .await?;
            tx.execute(ORPHAN_COLUMNS, &[&[agent].as_slice()]).await?;
            tx.commit().await?;
            Ok(ids.len())
        }

        async fn event(
            &self,
            agent: &str,
            entry: Option<i64>,
            kind: &str,
            score: Option<f32>,
        ) -> Result<()> {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO events (agent_id, entry_id, kind, score) VALUES ($1, $2, $3, $4)",
                &[&agent, &entry, &kind, &score],
            )
            .await?;
            Ok(())
        }

        async fn column_people(&self, agent: &str) -> Result<HashMap<ColumnKey, u32>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT site, database_id, name, count(*)::INT FROM column_people
                     WHERE agent_id = $1 GROUP BY 1, 2, 3",
                    &[&agent],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    (
                        (r.get(0), r.get(1), r.get(2)),
                        r.get::<_, i32>(3).max(0) as u32,
                    )
                })
                .collect())
        }

        async fn agents(&self) -> Result<Vec<String>> {
            let conn = self.pool.get().await?;
            Ok(conn
                .query(
                    "SELECT agent_id FROM entries UNION SELECT agent_id FROM column_people",
                    &[],
                )
                .await?
                .iter()
                .map(|r| r.get(0))
                .collect())
        }

        async fn mark_used(&self, agent: &str, ids: &[i64], shown: bool) -> Result<()> {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE entries SET uses = uses + $3::INT, last_seen = now() WHERE agent_id = $1 AND id = ANY($2)",
                &[&agent, &ids, &i32::from(shown)],
            )
            .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use workforce_postgres::test_support::with_test_pool;

        #[tokio::test(flavor = "multi_thread")]
        async fn reassigning_a_person_counts_them_once() {
            with_test_pool(PostgresKnowHowStore::build_pool, |pool| async move {
                let s = PostgresKnowHowStore::with_pool(pool).await.expect("store");
                super::super::reassign_person_contract(&s).await;
            })
            .await;
        }

        fn obs(person: &str, intent: Option<f32>) -> Observation {
            Observation {
                site: "mb".into(),
                database_id: 3,
                tables: vec!["public.orders".into()],
                columns: vec!["public.orders.status".into()],
                intent: intent.map(|x| IntentVector {
                    model: "m".into(),
                    vector: vec![x, 1.0],
                }),
                ..Observation::of(
                    Kind::DataLocation,
                    "data:mb:3:public.orders".into(),
                    person.into(),
                )
            }
        }

        fn code(person: &str) -> Observation {
            Observation {
                repo: Some("acme/app".into()),
                path: Some("src/a.rs".into()),
                symbol: Some("run".into()),
                ..Observation::of(
                    Kind::CodeLocation,
                    "code:acme/app:src/a.rs#run".into(),
                    person.into(),
                )
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn people_are_counted_once_and_forgetting_removes_their_part() {
            with_test_pool(PostgresKnowHowStore::build_pool, |pool| async move {
                let s = PostgresKnowHowStore::with_pool(pool).await.expect("store");
                let id = s.record("atlas", obs("p1", Some(0.1))).await.unwrap();
                assert_eq!(s.record("atlas", obs("p1", Some(0.2))).await.unwrap(), id);
                s.record("atlas", obs("p2", Some(0.3))).await.unwrap();
                let e = &s.entries("atlas").await.unwrap()[0];
                assert_eq!(e.people, 2, "the same person twice counts once");
                let cols = s.column_people("atlas").await.unwrap();
                assert_eq!(
                    cols[&("mb".to_string(), 3, "public.orders.status".to_string())],
                    2
                );
                assert!(s.entries("other-agent").await.unwrap().is_empty());
                assert_eq!(
                    s.intents("atlas", "m").await.unwrap().len(),
                    2,
                    "one intent per person"
                );
                assert!(s
                    .intents("atlas", "another-model")
                    .await
                    .unwrap()
                    .is_empty());
                for i in 0..20 {
                    s.record("atlas", obs("p3", Some(i as f32))).await.unwrap();
                }
                assert_eq!(
                    s.intents("atlas", "m").await.unwrap().len(),
                    3,
                    "one person cannot fill an entry's intents"
                );
                assert!(s.produced_by("atlas", "p2").await.unwrap().contains(&id));
                s.event("atlas", Some(id), "shadow", Some(0.8))
                    .await
                    .unwrap();
                s.mark_used("atlas", &[id], true).await.unwrap();
                s.mark_used("atlas", &[id], false).await.unwrap();
                assert_eq!(s.entries("atlas").await.unwrap()[0].uses, 1);

                s.forget_person("atlas", "p1").await.unwrap();
                s.forget_person("atlas", "p3").await.unwrap();
                let e = &s.entries("atlas").await.unwrap()[0];
                assert_eq!(e.people, 1);
                assert!(!s.produced_by("atlas", "p1").await.unwrap().contains(&id));
                s.forget_person("atlas", "p2").await.unwrap();
                assert!(s.entries("atlas").await.unwrap().is_empty(), "nobody left");

                let id = s.record("atlas", obs("p4", None)).await.unwrap();
                assert!(s.retire("atlas", id).await.unwrap());
                assert!(s.entries("atlas").await.unwrap().is_empty());
                s.record("atlas", obs("p5", Some(0.5))).await.unwrap();
                assert!(
                    s.entries("atlas").await.unwrap().is_empty(),
                    "a removed entry stays removed"
                );
                assert!(!s.set_pinned("atlas", id, true).await.unwrap());
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn forgetting_a_person_takes_the_columns_only_they_read() {
            with_test_pool(PostgresKnowHowStore::build_pool, |pool| async move {
                let s = PostgresKnowHowStore::with_pool(pool).await.expect("store");
                let id = s.record("atlas", obs("p1", None)).await.unwrap();
                let wider = Observation {
                    columns: vec!["public.orders.secret".into(), "public.orders.status".into()],
                    ..obs("p2", None)
                };
                s.record("atlas", wider).await.unwrap();
                s.set_pinned("atlas", id, true).await.unwrap();
                let added = s
                    .record(
                        "atlas",
                        Observation {
                            manual: true,
                            pin: true,
                            ..Observation::of(Kind::DataLocation, "manual:x".into(), "p2".into())
                        },
                    )
                    .await
                    .unwrap();

                s.forget_person("atlas", "p2").await.unwrap();
                let left = s.entries("atlas").await.unwrap();
                let e = left.iter().find(|e| e.id == id).unwrap();
                assert_eq!(e.columns, ["public.orders.status"]);
                assert!(!s
                    .column_people("atlas")
                    .await
                    .unwrap()
                    .keys()
                    .any(|(_, _, c)| c.ends_with(".secret")));
                assert!(
                    left.iter().any(|e| e.id == added),
                    "an addition an admin pinned stays"
                );

                s.forget_person("atlas", "p1").await.unwrap();
                assert!(
                    s.entries("atlas").await.unwrap().iter().all(|e| e.id != id),
                    "a pinned learned entry nobody produced goes"
                );
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn rechecks_revise_entries_and_unused_ones_are_pruned() {
            with_test_pool(PostgresKnowHowStore::build_pool, |pool| async move {
                let s = PostgresKnowHowStore::with_pool(pool).await.expect("store");
                let id = s.record("atlas", code("p1")).await.unwrap();
                let e = s.entries("atlas").await.unwrap().remove(0);
                assert_eq!(
                    (
                        e.kind,
                        e.repo.as_deref(),
                        e.path.as_deref(),
                        e.symbol.as_deref(),
                        e.stale
                    ),
                    (
                        Kind::CodeLocation,
                        Some("acme/app"),
                        Some("src/a.rs"),
                        Some("run"),
                        false
                    )
                );

                let checked = chrono::Utc::now();
                let moved = Revision {
                    id,
                    moved: Some(("code:acme/app:src/b.rs#run".into(), "src/b.rs".into())),
                    ..Revision::default()
                };
                assert!(s.revise("atlas", &moved, checked).await.unwrap());
                assert!(!s.revise("other-agent", &moved, checked).await.unwrap());
                let e = s.entries("atlas").await.unwrap().remove(0);
                assert_eq!(e.path.as_deref(), Some("src/b.rs"));
                assert_eq!(e.key, "code:acme/app:src/b.rs#run");
                let stale = Revision {
                    id,
                    stale: true,
                    ..Revision::default()
                };
                assert!(s.revise("atlas", &stale, checked).await.unwrap());
                assert!(s.entries("atlas").await.unwrap()[0].stale);

                let taken = s
                    .record(
                        "atlas",
                        Observation {
                            key: "code:acme/app:src/c.rs#run".into(),
                            path: Some("src/c.rs".into()),
                            ..code("p1")
                        },
                    )
                    .await
                    .unwrap();
                s.retire("atlas", taken).await.unwrap();
                let onto_taken = Revision {
                    id,
                    moved: Some(("code:acme/app:src/c.rs#run".into(), "src/c.rs".into())),
                    ..Revision::default()
                };
                assert!(s
                    .revise("atlas", &onto_taken, chrono::Utc::now())
                    .await
                    .unwrap());
                let e = s.entries("atlas").await.unwrap().remove(0);
                assert_eq!(
                    (e.path.as_deref(), e.stale),
                    (Some("src/b.rs"), true),
                    "where it moved is taken, even by a retired entry: stale, not moved"
                );

                let again = Observation {
                    key: e.key.clone(),
                    path: e.path.clone(),
                    ..code("p2")
                };
                assert_eq!(s.record("atlas", again).await.unwrap(), id);
                let e = s.entries("atlas").await.unwrap().remove(0);
                assert!(!e.stale, "produced again: no longer stale");
                assert_eq!(e.people, 2);
                assert!(s.revise("atlas", &stale, checked).await.unwrap());
                assert!(
                    !s.entries("atlas").await.unwrap()[0].stale,
                    "a turn produced it after the re-check read it"
                );

                let data = s.record("atlas", obs("p1", None)).await.unwrap();
                s.set_pinned("atlas", data, true).await.unwrap();
                let later = chrono::Utc::now() + chrono::Duration::minutes(1);
                assert_eq!(
                    s.prune_unused(later).await.unwrap(),
                    1,
                    "the pinned one stays"
                );
                let left = s.entries("atlas").await.unwrap();
                assert_eq!(left.len(), 1);
                assert_eq!(left[0].id, data);

                assert!(s.delete("atlas", data).await.unwrap());
                assert!(!s.delete("atlas", data).await.unwrap());
                assert!(s.entries("atlas").await.unwrap().is_empty());
                assert!(
                    s.column_people("atlas").await.unwrap().is_empty(),
                    "once no entry names a column, who read it goes too"
                );
                s.record("atlas", obs("p1", None)).await.unwrap();
                assert_eq!(
                    s.entries("atlas").await.unwrap().len(),
                    1,
                    "a deleted entry can be learned again"
                );
            })
            .await;
        }
    }
}
