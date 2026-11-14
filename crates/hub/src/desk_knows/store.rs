//! Where what a person's Desk shares with one agent here is kept: one row
//! per thing shared, by `(person, source, agent, kind, ref)`, upserted as
//! Desk sends it again, deleted when Desk forgets it, pruned by age and
//! size. `source` is the minting device, so two Desks of one person never
//! overwrite each other. Postgres with its own schema and migrations
//! (`migrations_knows`), or in memory for tests and hubs without Postgres
//! that only want the routes to answer.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// One thing a Desk shared.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: String,
    pub reference: String,
    pub session: Option<String>,
    pub group_name: Option<String>,
    pub at: DateTime<Utc>,
    /// Plain fields; `text` is what is searched.
    pub body: Value,
}

impl Item {
    pub fn bytes(&self) -> i32 {
        i32::try_from(self.body.to_string().len()).unwrap_or(i32::MAX)
    }
}

/// Whose a row is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner {
    pub person: Uuid,
    /// The minting device's id.
    pub source: String,
    pub agent: String,
}

/// What a forget covers: the items named, every item of the sessions
/// named, every item of the kinds named, or everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forget {
    pub items: Vec<(String, String)>,
    pub sessions: Vec<String>,
    pub kinds: Vec<String>,
    pub all: bool,
}

/// What is kept for one owner, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub kinds: BTreeMap<String, KindCount>,
    pub bytes: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KindCount {
    pub items: i64,
    pub bytes: i64,
}

#[async_trait]
pub trait KnowsStore: Send + Sync {
    /// Keeps `items` for `owner`, each replacing what it had under its kind
    /// and ref. Answers how many were kept.
    async fn push(&self, owner: &Owner, items: &[Item], now: DateTime<Utc>) -> Result<u64>;
    /// Deletes what `forget` covers of `owner`'s. Answers how many went.
    async fn forget(&self, owner: &Owner, forget: &Forget) -> Result<u64>;
    async fn status(&self, owner: &Owner) -> Result<Status>;
    /// Drops conversations kept since before `before`, then each person's
    /// conversations for an agent past `max_bytes`, oldest first. Answers
    /// how many went.
    async fn prune(&self, before: DateTime<Utc>, max_bytes: i64) -> Result<u64>;
    /// Everything kept for `person`, gone.
    async fn forget_person(&self, person: Uuid) -> Result<u64>;
}

/// The kind a session's conversation is shared as.
pub const TRANSCRIPT: &str = "transcript";

// ── In memory ────────────────────────────────────────────────────────────

/// Each item by whose it is, its kind and ref, with when it came.
type Kept = BTreeMap<(Owner, String, String), (Item, DateTime<Utc>)>;

#[derive(Default)]
pub struct MemoryKnows(Mutex<Kept>);

impl MemoryKnows {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every item kept for `owner`, for tests.
    pub fn items(&self, owner: &Owner) -> Vec<Item> {
        self.0
            .lock()
            .iter()
            .filter(|((o, _, _), _)| o == owner)
            .map(|(_, (i, _))| i.clone())
            .collect()
    }
}

fn covered(forget: &Forget, item: &Item) -> bool {
    forget.all
        || forget
            .items
            .iter()
            .any(|(k, r)| *k == item.kind && *r == item.reference)
        || item
            .session
            .as_ref()
            .is_some_and(|s| forget.sessions.contains(s))
        || forget.kinds.contains(&item.kind)
}

#[async_trait]
impl KnowsStore for MemoryKnows {
    async fn push(&self, owner: &Owner, items: &[Item], now: DateTime<Utc>) -> Result<u64> {
        let mut kept = self.0.lock();
        for i in items {
            kept.insert(
                (owner.clone(), i.kind.clone(), i.reference.clone()),
                (i.clone(), now),
            );
        }
        Ok(items.len() as u64)
    }

    async fn forget(&self, owner: &Owner, forget: &Forget) -> Result<u64> {
        let mut kept = self.0.lock();
        let before = kept.len();
        kept.retain(|(o, _, _), (i, _)| !(o == owner && covered(forget, i)));
        Ok((before - kept.len()) as u64)
    }

    async fn status(&self, owner: &Owner) -> Result<Status> {
        let mut s = Status::default();
        for ((o, kind, _), (i, _)) in self.0.lock().iter() {
            if o == owner {
                let c = s.kinds.entry(kind.clone()).or_default();
                c.items += 1;
                c.bytes += i64::from(i.bytes());
                s.bytes += i64::from(i.bytes());
            }
        }
        Ok(s)
    }

    async fn prune(&self, before: DateTime<Utc>, max_bytes: i64) -> Result<u64> {
        let mut kept = self.0.lock();
        let start = kept.len();
        kept.retain(|(_, kind, _), (i, _)| !(kind == TRANSCRIPT && i.at < before));
        // Each (person, agent)'s items past the cap: conversations first,
        // oldest first, then the rest, oldest first.
        let mut by_owner: BTreeMap<
            (Uuid, String),
            Vec<((Owner, String, String), (bool, DateTime<Utc>), i64)>,
        > = BTreeMap::new();
        for (k, (i, _)) in kept.iter() {
            by_owner
                .entry((k.0.person, k.0.agent.clone()))
                .or_default()
                .push((k.clone(), (k.1 != TRANSCRIPT, i.at), i64::from(i.bytes())));
        }
        for (_, mut rows) in by_owner {
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            let mut total = 0;
            for (k, _, bytes) in rows {
                total += bytes;
                if total > max_bytes {
                    kept.remove(&k);
                }
            }
        }
        Ok((start - kept.len()) as u64)
    }

    async fn forget_person(&self, person: Uuid) -> Result<u64> {
        let mut kept = self.0.lock();
        let before = kept.len();
        kept.retain(|(o, _, _), _| o.person != person);
        Ok((before - kept.len()) as u64)
    }
}

// ── Postgres ─────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use eyre::Context;
    use workforce_postgres::{
        MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
    };

    // Own directory, never a subdir of another embed root: refinery's
    // embed_migrations is recursive.
    mod embedded {
        refinery::embed_migrations!("migrations_knows");
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

    const SEARCH_PATH: &str = "SET search_path TO desk_knows, public";

    pub struct PostgresKnows {
        pool: PostgresPool,
    }

    impl PostgresKnows {
        pub const SCHEMA: &'static str = "desk_knows";

        pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
            PostgresPool::from_config_with_session_init(&config.small(), &[SEARCH_PATH])
        }

        pub async fn new(config: &PostgresConfig) -> Result<Self> {
            let pool = Self::build_pool(config).context("building desk knows pool")?;
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
            .context("running desk knows migrations")?;
            Ok(Self { pool })
        }
    }

    #[async_trait]
    impl KnowsStore for PostgresKnows {
        async fn push(&self, owner: &Owner, items: &[Item], now: DateTime<Utc>) -> Result<u64> {
            let mut client = self.pool.get().await?;
            let tx = client.transaction().await?;
            let stmt = tx
                .prepare(
                    "INSERT INTO items (person, source, agent, kind, ref, session, group_name, at, body, bytes, received_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                     ON CONFLICT (person, source, agent, kind, ref) DO UPDATE SET \
                       session = EXCLUDED.session, group_name = EXCLUDED.group_name, at = EXCLUDED.at, \
                       body = EXCLUDED.body, bytes = EXCLUDED.bytes, received_at = EXCLUDED.received_at",
                )
                .await?;
            for i in items {
                tx.execute(
                    &stmt,
                    &[
                        &owner.person,
                        &owner.source,
                        &owner.agent,
                        &i.kind,
                        &i.reference,
                        &i.session,
                        &i.group_name,
                        &i.at,
                        &i.body,
                        &i.bytes(),
                        &now,
                    ],
                )
                .await?;
            }
            tx.commit().await?;
            Ok(items.len() as u64)
        }

        async fn forget(&self, owner: &Owner, forget: &Forget) -> Result<u64> {
            let client = self.pool.get().await?;
            let (kinds, refs): (Vec<&str>, Vec<&str>) = forget
                .items
                .iter()
                .map(|(k, r)| (k.as_str(), r.as_str()))
                .unzip();
            Ok(client
                .execute(
                    "DELETE FROM items WHERE person = $1 AND source = $2 AND agent = $3 AND ( \
                       $4 OR (kind, ref) IN (SELECT * FROM unnest($5::text[], $6::text[])) \
                       OR session = ANY($7::text[]) OR kind = ANY($8::text[]))",
                    &[
                        &owner.person,
                        &owner.source,
                        &owner.agent,
                        &forget.all,
                        &kinds,
                        &refs,
                        &forget.sessions,
                        &forget.kinds,
                    ],
                )
                .await?)
        }

        async fn status(&self, owner: &Owner) -> Result<Status> {
            let client = self.pool.get().await?;
            let mut s = Status::default();
            for r in client
                .query(
                    "SELECT kind, count(*), coalesce(sum(bytes), 0)::bigint FROM items \
                     WHERE person = $1 AND source = $2 AND agent = $3 GROUP BY kind",
                    &[&owner.person, &owner.source, &owner.agent],
                )
                .await?
            {
                let c = KindCount {
                    items: r.try_get(1)?,
                    bytes: r.try_get(2)?,
                };
                s.bytes += c.bytes;
                s.kinds.insert(r.try_get(0)?, c);
            }
            Ok(s)
        }

        async fn prune(&self, before: DateTime<Utc>, max_bytes: i64) -> Result<u64> {
            let client = self.pool.get().await?;
            let old = client
                .execute(
                    "DELETE FROM items WHERE kind = $1 AND at < $2",
                    &[&TRANSCRIPT, &before],
                )
                .await?;
            let over = client
                .execute(
                    "DELETE FROM items i USING ( \
                       SELECT person, source, agent, kind, ref, \
                         sum(bytes) OVER (PARTITION BY person, agent \
                           ORDER BY (kind <> $1) DESC, at DESC, kind, ref) AS running \
                       FROM items) o \
                     WHERE o.running > $2 AND i.person = o.person AND i.source = o.source \
                       AND i.agent = o.agent AND i.kind = o.kind AND i.ref = o.ref",
                    &[&TRANSCRIPT, &max_bytes],
                )
                .await?;
            Ok(old + over)
        }

        async fn forget_person(&self, person: Uuid) -> Result<u64> {
            let client = self.pool.get().await?;
            Ok(client
                .execute("DELETE FROM items WHERE person = $1", &[&person])
                .await?)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use workforce_postgres::test_support::with_test_pool;

        #[tokio::test]
        async fn kept_upserted_forgotten_counted_and_pruned_in_postgres() {
            with_test_pool(PostgresKnows::build_pool, |pool| async move {
                let store = PostgresKnows::with_pool(pool).await.unwrap();
                super::super::tests::exercise(&store).await;
            })
            .await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn item(kind: &str, reference: &str, session: &str, at: DateTime<Utc>) -> Item {
        Item {
            kind: kind.into(),
            reference: reference.into(),
            session: Some(session.into()),
            group_name: Some("Patchwork".into()),
            at,
            body: serde_json::json!({ "text": format!("{kind} {reference} of {session}") }),
        }
    }

    pub(crate) async fn exercise(store: &dyn KnowsStore) {
        let now = Utc::now();
        let allan = Owner {
            person: Uuid::from_u128(1),
            source: "mac".into(),
            agent: "allan".into(),
        };
        let other = Owner {
            person: Uuid::from_u128(2),
            ..allan.clone()
        };
        let old = now - chrono::Duration::days(200);
        store
            .push(
                &allan,
                &[
                    item("account", "a1", "s1", now),
                    item(TRANSCRIPT, "x1", "s1", old),
                    item(TRANSCRIPT, "x2", "s2", now),
                    item(TRANSCRIPT, "x3", "s2", now - chrono::Duration::hours(1)),
                ],
                now,
            )
            .await
            .unwrap();
        store
            .push(&other, &[item("account", "a1", "s9", now)], now)
            .await
            .unwrap();
        // Sent again: replaced, once.
        let mut again = item("account", "a1", "s1", now);
        again.body = serde_json::json!({ "text": "newer" });
        let again_bytes = again.bytes();
        store.push(&allan, &[again], now).await.unwrap();
        let s = store.status(&allan).await.unwrap();
        assert_eq!(s.kinds["account"].items, 1);
        assert_eq!(s.kinds[TRANSCRIPT].items, 3);
        // Prune by age: the 200-day-old conversation goes; then by size,
        // with room for the account and one conversation, the older of the
        // two left goes: conversations go first, the account stays.
        let x = i64::from(item(TRANSCRIPT, "x2", "s2", now).bytes());
        let account = i64::from(again_bytes);
        assert_eq!(
            store
                .prune(now - chrono::Duration::days(90), x + account)
                .await
                .unwrap(),
            2
        );
        let s = store.status(&allan).await.unwrap();
        assert_eq!(s.kinds[TRANSCRIPT].items, 1);
        assert_eq!(s.kinds["account"].items, 1);
        // Forget by item, by session, then the rest by kind.
        store
            .push(&allan, &[item(TRANSCRIPT, "x4", "s3", now)], now)
            .await
            .unwrap();
        let by_session = Forget {
            sessions: vec!["s3".into()],
            ..Forget::default()
        };
        assert_eq!(store.forget(&allan, &by_session).await.unwrap(), 1);
        let by_item = Forget {
            items: vec![(TRANSCRIPT.into(), "x2".into())],
            ..Forget::default()
        };
        assert_eq!(store.forget(&allan, &by_item).await.unwrap(), 1);
        let by_kind = Forget {
            kinds: vec!["account".into()],
            ..Forget::default()
        };
        assert_eq!(store.forget(&allan, &by_kind).await.unwrap(), 1);
        assert_eq!(store.status(&allan).await.unwrap().bytes, 0);
        // Another person's is untouched by all of it, until they go.
        assert_eq!(
            store.status(&other).await.unwrap().kinds["account"].items,
            1
        );
        assert_eq!(store.forget_person(other.person).await.unwrap(), 1);
        assert_eq!(store.status(&other).await.unwrap(), Status::default());
    }

    #[tokio::test]
    async fn kept_upserted_forgotten_counted_and_pruned_in_memory() {
        exercise(MemoryKnows::new().as_ref()).await;
    }
}
