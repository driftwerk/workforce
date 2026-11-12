//! Sessions' running accounts in Postgres: read, written only on the
//! version read (compare-and-set), the exchanges of a session's
//! conversation not yet read (whichever session harvested them), and the
//! cheap model's day count shared by everything that asks it.

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};
use uuid::Uuid;

use crate::account::{Account, Cursor, Exchange};
use crate::memory::record::{Asker, Step};
use crate::store::DeskStore;

/// What the cheap model's calls are counted as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheapKind {
    /// Refreshing an account.
    Account,
    /// Answering a question about a session from its exchanges.
    Question,
}

impl CheapKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Question => "question",
        }
    }
}

/// How a write of an account went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// Kept, at this version.
    Kept(i64),
    /// Another write landed first: the account is no longer at the version
    /// read.
    Lost,
}

const COLUMNS: &str = "session, owner, purpose, asked_by, plan, done, now, waiting_on, next, \
     decisions, cursor, version, written_at, by_model, stale_after";

fn account_of(r: &tokio_postgres::Row) -> Result<Account> {
    let json = |i: usize| -> Result<serde_json::Value> { Ok(r.try_get(i)?) };
    Ok(Account {
        session: r.try_get(0)?,
        owner: r.try_get(1)?,
        purpose: r.try_get(2)?,
        asked_by: r.try_get(3)?,
        plan: r.try_get(4)?,
        done: serde_json::from_value(json(5)?).unwrap_or_default(),
        now: r.try_get(6)?,
        waiting_on: r.try_get(7)?,
        next: r.try_get(8)?,
        decisions: serde_json::from_value(json(9)?).unwrap_or_default(),
        cursor: serde_json::from_value::<Cursor>(json(10)?).unwrap_or_default(),
        version: r.try_get(11)?,
        written_at: r.try_get(12)?,
        by_model: r.try_get(13)?,
        stale_after: r.try_get(14)?,
    })
}

impl DeskStore {
    /// `session`'s account, if one is kept.
    pub async fn session_account(&self, session: Uuid) -> Result<Option<Account>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM session_accounts WHERE session = $1"),
                &[&session],
            )
            .await?
            .map(|r| account_of(&r))
            .transpose()
    }

    /// The accounts of `sessions` that are kept.
    pub async fn session_accounts(&self, sessions: &[Uuid]) -> Result<Vec<Account>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {COLUMNS} FROM session_accounts WHERE session = ANY($1)"),
                &[&sessions],
            )
            .await?
            .iter()
            .map(account_of)
            .collect()
    }

    /// Keeps `a` if it is still at `a.version` (0: none kept yet), moving it
    /// to the next version; a write that finds it moved on is [`Put::Lost`]
    /// and changes nothing.
    pub async fn session_account_put(&self, a: &Account) -> Result<Put> {
        let client = self.pool().get().await?;
        let done = serde_json::to_value(&a.done)?;
        let decisions = serde_json::to_value(&a.decisions)?;
        let cursor = serde_json::to_value(&a.cursor)?;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 14] = [
            &a.session,
            &a.owner,
            &a.purpose,
            &a.asked_by,
            &a.plan,
            &done,
            &a.now,
            &a.waiting_on,
            &a.next,
            &decisions,
            &cursor,
            &a.written_at,
            &a.by_model,
            &a.stale_after,
        ];
        let row = match a.version {
            0 => client
                .query_opt(
                    "INSERT INTO session_accounts (session, owner, purpose, asked_by, plan, done, \
                       now, waiting_on, next, decisions, cursor, written_at, by_model, \
                       stale_after, version) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 1) \
                     ON CONFLICT (session) DO NOTHING RETURNING version",
                    &params,
                )
                .await
                .context("keeping a session's first account")?,
            v => {
                let mut with: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params.to_vec();
                with.push(&v);
                client
                    .query_opt(
                        "UPDATE session_accounts SET owner = $2, purpose = $3, asked_by = $4, \
                           plan = $5, done = $6, now = $7, waiting_on = $8, next = $9, \
                           decisions = $10, cursor = $11, written_at = $12, by_model = $13, \
                           stale_after = $14, version = version + 1 \
                         WHERE session = $1 AND version = $15 RETURNING version",
                        &with,
                    )
                    .await
                    .context("keeping a session's account")?
            }
        };
        Ok(match row {
            Some(r) => Put::Kept(r.try_get(0)?),
            None => Put::Lost,
        })
    }

    /// The exchanges of `session`'s conversation begun since `since` (all
    /// with none), newest first, at most `limit`: those it harvested itself
    /// and those its record holds that another session kept first (a
    /// resumed or forked conversation's history). Each with whether it was
    /// flagged as reading like instructions.
    pub async fn account_exchanges(
        &self,
        session: Uuid,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<(Exchange, bool)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT e.key, e.at, e.ended, e.asker, e.ask, e.answer, e.steps, e.errors, \
                   cardinality(e.flagged) > 0 \
                 FROM memory_exchanges e \
                 WHERE (e.session = $1 OR EXISTS (SELECT 1 FROM memory_record_keys k \
                     WHERE k.session = $1 AND k.engine = e.engine AND k.key = e.key)) \
                   AND ($2::timestamptz IS NULL OR e.at >= $2) AND NOT e.compaction \
                 ORDER BY e.at DESC, e.seq DESC LIMIT $3",
                &[&session, &since, &limit],
            )
            .await
            .context("reading a session's exchanges for its account")?
            .iter()
            .map(|r| {
                let steps: serde_json::Value = r.try_get(6)?;
                Ok((
                    Exchange {
                        key: r.try_get(0)?,
                        at: r.try_get(1)?,
                        ended: r.try_get(2)?,
                        asker: Asker::parse(r.try_get::<_, &str>(3)?).unwrap_or(Asker::Unknown),
                        ask: r.try_get(4)?,
                        answer: r.try_get(5)?,
                        steps: serde_json::from_value::<Vec<Step>>(steps).unwrap_or_default(),
                        errors: r.try_get(7)?,
                    },
                    r.try_get(8)?,
                ))
            })
            .collect()
    }

    /// The cheap model's calls on `day`, of every kind.
    pub async fn cheap_calls(&self, day: NaiveDate) -> Result<i64> {
        let client = self.pool().get().await?;
        Ok(client
            .query_one(
                "SELECT COALESCE(SUM(calls), 0)::int8 FROM desk_cheap_calls WHERE day = $1",
                &[&day],
            )
            .await?
            .try_get(0)?)
    }

    /// Counts a call of `kind` on `day`, unless `cap` calls of every kind
    /// are made already, or `kind_cap` of this kind. Whether it may be made.
    pub async fn cheap_take(
        &self,
        day: NaiveDate,
        kind: CheapKind,
        cap: u32,
        kind_cap: Option<u32>,
    ) -> Result<bool> {
        if cap == 0 || kind_cap == Some(0) {
            return Ok(false);
        }
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // One count at a time, so two takers can't both pass the cap.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('desk_cheap_calls'))",
            &[],
        )
        .await?;
        let (all, mine): (i64, i64) = {
            let r = tx
                .query_one(
                    "SELECT COALESCE(SUM(calls), 0)::int8, \
                       COALESCE(SUM(calls) FILTER (WHERE kind = $2), 0)::int8 \
                     FROM desk_cheap_calls WHERE day = $1",
                    &[&day, &kind.as_str()],
                )
                .await?;
            (r.try_get(0)?, r.try_get(1)?)
        };
        if all >= i64::from(cap) || kind_cap.is_some_and(|k| mine >= i64::from(k)) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO desk_cheap_calls (day, kind, calls) VALUES ($1, $2, 1) \
             ON CONFLICT (day, kind) DO UPDATE SET calls = desk_cheap_calls.calls + 1",
            &[&day, &kind.as_str()],
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Gives back a call of `kind` counted on `day` that no model took.
    pub async fn cheap_untake(&self, day: NaiveDate, kind: CheapKind) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE desk_cheap_calls SET calls = GREATEST(calls - 1, 0) \
                 WHERE day = $1 AND kind = $2",
                &[&day, &kind.as_str()],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::account::Read;
    use crate::engine::Engine;
    use crate::ledger_store::Cursor as LedgerCursor;
    use crate::memory::record::Exchange as Kept;
    use crate::memory_store::RecordWrite;
    use crate::store::{build_pool, tests::record_fixture};

    fn kept(key: &str, minutes_ago: i64) -> Kept {
        let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
        Kept {
            key: key.into(),
            at,
            ended: Some(at + chrono::Duration::seconds(30)),
            asker: Asker::Person,
            ask: format!("ask {key}"),
            answer: format!("answer {key}"),
            steps: Vec::new(),
            files: Default::default(),
            commands: Vec::new(),
            errors: Vec::new(),
            compaction: false,
            redacted: 0,
            flagged: Default::default(),
        }
    }

    async fn harvest(store: &DeskStore, session: Uuid, source: &str, closed: &[Kept]) {
        store
            .record_write(&RecordWrite {
                session,
                engine: Engine::Claude,
                source,
                cursor: &LedgerCursor::default(),
                rows: &[],
                closed,
                open: None,
                project: "/dev/acme-web",
                folder: "/dev/acme-web",
                name: "acme-web",
                cli_session_id: None,
            })
            .await
            .unwrap();
    }

    async fn session(store: &DeskStore, name: &str) -> Uuid {
        let r = record_fixture(name);
        store.put(&r).await.unwrap();
        r.id
    }

    /// The keys a refresh would read now, oldest first, and the cursor
    /// after it.
    async fn unread(store: &DeskStore, s: Uuid, c: &Cursor) -> (Vec<String>, Cursor) {
        let rows = store.account_exchanges(s, c.since(), 100).await.unwrap();
        let mut keys: Vec<(String, DateTime<Utc>)> = rows
            .into_iter()
            .filter(|(x, _)| c.unread(&x.key, x.at))
            .map(|(x, _)| (x.key, x.at))
            .collect();
        keys.reverse();
        let next = c.with(keys.iter().map(|(k, at)| Read {
            key: k.clone(),
            at: *at,
        }));
        (keys.into_iter().map(|(k, _)| k).collect(), next)
    }

    #[tokio::test]
    async fn the_cursor_reads_a_resumed_or_forked_sessions_exchanges_once() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let first = session(&store, "first").await;
            let fork = session(&store, "fork").await;
            harvest(
                &store,
                first,
                "/records/a.jsonl",
                &[kept("a1", 30), kept("a2", 20)],
            )
            .await;
            // The fork's record copies the first's history, then goes on:
            // the copied exchanges stay the first's.
            harvest(
                &store,
                fork,
                "/records/b.jsonl",
                &[kept("a1", 30), kept("a2", 20), kept("b1", 10)],
            )
            .await;
            let owners: i64 = store
                .pool()
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1",
                    &[&fork],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(owners, 1, "the copied history is kept by the first session");
            let (keys, c) = unread(&store, fork, &Cursor::default()).await;
            assert_eq!(keys, ["a1", "a2", "b1"], "its whole conversation, once");
            let (again, c) = unread(&store, fork, &c).await;
            assert!(again.is_empty(), "nothing is read twice: {again:?}");
            harvest(&store, fork, "/records/b.jsonl", &[kept("b2", 5)]).await;
            let (keys, _) = unread(&store, fork, &c).await;
            assert_eq!(keys, ["b2"]);
            // A resumed session reading the same record finds the first's.
            let resumed = session(&store, "resumed").await;
            harvest(
                &store,
                resumed,
                "/records/a.jsonl",
                &[kept("a1", 30), kept("a2", 20), kept("c1", 1)],
            )
            .await;
            let (keys, _) = unread(&store, resumed, &Cursor::default()).await;
            assert_eq!(keys, ["a1", "a2", "c1"]);
        })
        .await;
    }

    #[tokio::test]
    async fn a_write_on_a_version_that_moved_on_is_lost_and_changes_nothing() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let s = Uuid::new_v4();
            let mut a = Account::new(s, "me", crate::crew::now());
            a.now = "first".into();
            assert_eq!(store.session_account_put(&a).await.unwrap(), Put::Kept(1));
            // A second first write loses.
            let mut b = Account::new(s, "me", crate::crew::now());
            b.now = "rival".into();
            assert_eq!(store.session_account_put(&b).await.unwrap(), Put::Lost);
            let kept = store.session_account(s).await.unwrap().unwrap();
            assert_eq!((kept.now.as_str(), kept.version), ("first", 1));
            // Two writers that read version 1: the second loses.
            let mut one = kept.clone();
            one.now = "one".into();
            let mut two = kept.clone();
            two.now = "two".into();
            assert_eq!(store.session_account_put(&one).await.unwrap(), Put::Kept(2));
            assert_eq!(store.session_account_put(&two).await.unwrap(), Put::Lost);
            assert_eq!(store.session_account(s).await.unwrap().unwrap().now, "one");
        })
        .await;
    }

    #[tokio::test]
    async fn the_day_count_holds_every_kind_to_the_ceiling_and_questions_to_their_share() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let day = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
            assert!(store
                .cheap_take(day, CheapKind::Question, 3, Some(1))
                .await
                .unwrap());
            assert!(
                !store
                    .cheap_take(day, CheapKind::Question, 3, Some(1))
                    .await
                    .unwrap(),
                "past the questions' share"
            );
            assert!(store
                .cheap_take(day, CheapKind::Account, 3, None)
                .await
                .unwrap());
            assert!(store
                .cheap_take(day, CheapKind::Account, 3, None)
                .await
                .unwrap());
            assert!(
                !store
                    .cheap_take(day, CheapKind::Account, 3, None)
                    .await
                    .unwrap(),
                "past the ceiling"
            );
            store.cheap_untake(day, CheapKind::Account).await.unwrap();
            assert_eq!(store.cheap_calls(day).await.unwrap(), 2);
            assert!(store
                .cheap_take(day, CheapKind::Account, 3, None)
                .await
                .unwrap());
        })
        .await;
    }
}
