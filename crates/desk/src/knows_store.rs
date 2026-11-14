//! What each agent knows of the work, in Postgres: its rule
//! (`knows_rules`), what was shared with it (`knows_sent`), how far each
//! kind has gone (`knows_cursors`), and the person's changes and choices
//! (`knows_log`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::knows::{Digest, Held, Rule};
use crate::store::DeskStore;

/// An agent's rule as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRule {
    pub agent: String,
    pub rule: Rule,
    pub revision: i64,
    pub changed_at: DateTime<Utc>,
}

/// One line of an agent's log.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub what: String,
    pub detail: Value,
    pub done_at: Option<DateTime<Utc>>,
}

/// One thing shared with an agent, to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub held: Held,
    pub bytes: i32,
    /// What was sent of a kind sent again when it changes (an account, a
    /// need, a decision): its body's hash. None for one sent once.
    pub hash: Option<String>,
}

/// What `agent` was sent of one item, as the sync reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentState {
    pub hash: Option<String>,
    /// It is being deleted: never sent again until that is done.
    pub deleting: bool,
    /// It was deleted: sent again only as new.
    pub deleted: bool,
}

/// The longest log an agent's page reads at once.
pub const LOG_PAGE: i64 = 200;

fn kept(r: &Row) -> Result<KeptRule> {
    let places: Vec<String> = r.try_get(1)?;
    let digest: String = r.try_get(3)?;
    // A place kept by an older or newer Desk that this one can't read is
    // left out, never widened into something else.
    let readable: Vec<String> = places
        .into_iter()
        .filter(|p| crate::knows::Place::parse(p).is_ok())
        .collect();
    Ok(KeptRule {
        agent: r.try_get(0)?,
        rule: Rule::new(
            &readable,
            r.try_get(2)?,
            Digest::parse(&digest).unwrap_or_default(),
        )
        .map_err(|e| eyre::eyre!(e))?,
        revision: r.try_get(4)?,
        changed_at: r.try_get(5)?,
    })
}

const RULE_COLUMNS: &str = "agent, places, transcripts, digest, revision, changed_at";

impl DeskStore {
    /// Every agent's rule.
    pub async fn knows_rules(&self) -> Result<Vec<KeptRule>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {RULE_COLUMNS} FROM knows_rules ORDER BY agent"),
                &[],
            )
            .await?
            .iter()
            .map(kept)
            .collect()
    }

    /// The agent `agent`'s rule, when it has one.
    pub async fn knows_rule(&self, agent: &str) -> Result<Option<KeptRule>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {RULE_COLUMNS} FROM knows_rules WHERE agent = $1"),
                &[&agent],
            )
            .await?
            .as_ref()
            .map(kept)
            .transpose()
    }

    /// Keeps `rule` for `agent`, the revision after the one it had, and logs
    /// the change with the places it names, in one transaction.
    pub async fn knows_rule_put(
        &self,
        agent: &str,
        rule: &Rule,
        now: DateTime<Utc>,
    ) -> Result<KeptRule> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO knows_rules (agent, places, transcripts, digest, revision, changed_at) \
                     VALUES ($1, $2, $3, $4, 1, $5) \
                     ON CONFLICT (agent) DO UPDATE SET places = EXCLUDED.places, \
                       transcripts = EXCLUDED.transcripts, digest = EXCLUDED.digest, \
                       revision = knows_rules.revision + 1, changed_at = EXCLUDED.changed_at \
                     RETURNING {RULE_COLUMNS}"
                ),
                &[
                    &agent,
                    &rule.texts(),
                    &rule.transcripts,
                    &rule.digest.as_str(),
                    &now,
                ],
            )
            .await?;
        let detail = serde_json::json!({
            "places": rule.texts(),
            "transcripts": rule.transcripts,
            "digest": rule.digest.as_str(),
        });
        tx.execute(
            "INSERT INTO knows_log (id, at, agent, what, detail, done_at) VALUES ($1, $2, $3, 'rule', $4, $2)",
            &[&Uuid::new_v4(), &now, &agent, &detail],
        )
        .await?;
        tx.commit().await?;
        kept(&row)
    }

    /// What `agent` holds now that the person hasn't settled: everything
    /// shared with it, not deleted, not being deleted, and not kept by the
    /// person's choice.
    pub async fn knows_held(&self, agent: &str) -> Result<Vec<Held>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT kind, ref, session, group_id, board, boss FROM knows_sent \
                 WHERE agent = $1 AND deleted_at IS NULL AND deleting IS NULL AND kept_at IS NULL",
                &[&agent],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(Held {
                    kind: r.try_get(0)?,
                    reference: r.try_get(1)?,
                    session: r.try_get(2)?,
                    group: r.try_get(3)?,
                    board: r.try_get(4)?,
                    boss: r.try_get(5)?,
                })
            })
            .collect()
    }

    /// Records what was shared with `agent`: each item once, its size and
    /// time renewed when sent again, and held again (no longer kept by
    /// choice) if it was deleted or kept. A pending delete stays: the sync
    /// never sends what is being deleted ([`Self::knows_deleting`]).
    pub async fn knows_sent_put(
        &self,
        agent: &str,
        sent: &[Sent],
        now: DateTime<Utc>,
    ) -> Result<()> {
        if sent.is_empty() {
            return Ok(());
        }
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let stmt = tx
            .prepare(
                "INSERT INTO knows_sent (agent, kind, ref, session, group_id, board, boss, bytes, sent_at, hash) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (agent, kind, ref) DO UPDATE SET session = EXCLUDED.session, \
                   group_id = EXCLUDED.group_id, board = EXCLUDED.board, boss = EXCLUDED.boss, \
                   bytes = EXCLUDED.bytes, sent_at = EXCLUDED.sent_at, hash = EXCLUDED.hash, \
                   deleted_at = NULL, kept_at = NULL",
            )
            .await?;
        for s in sent {
            let h = &s.held;
            tx.execute(
                &stmt,
                &[
                    &agent,
                    &h.kind,
                    &h.reference,
                    &h.session,
                    &h.group,
                    &h.board,
                    &h.boss,
                    &s.bytes,
                    &now,
                    &s.hash,
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// What `agent` was sent of `kinds`, by (kind, ref): for the sync to
    /// send a changed item again, and nothing being deleted.
    pub async fn knows_sent_states(
        &self,
        agent: &str,
        kinds: &[&str],
    ) -> Result<HashMap<(String, String), SentState>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT kind, ref, hash, deleting IS NOT NULL, deleted_at IS NOT NULL FROM knows_sent \
                 WHERE agent = $1 AND kind = ANY($2)",
                &[&agent, &kinds],
            )
            .await?
            .iter()
            .map(|r| {
                Ok((
                    (r.try_get(0)?, r.try_get(1)?),
                    SentState {
                        hash: r.try_get(2)?,
                        deleting: r.try_get(3)?,
                        deleted: r.try_get(4)?,
                    },
                ))
            })
            .collect()
    }

    /// What was sent of a forgotten session after it was forgotten (a push
    /// in flight then): marked for deleting as the forgetting did.
    pub async fn knows_forget_late(&self) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let ids: Vec<Uuid> = tx
            .query(
                "SELECT DISTINCT k.session FROM knows_sent k \
                 JOIN knows_forgotten f ON f.session = k.session \
                 WHERE k.deleted_at IS NULL AND k.deleting IS NULL",
                &[],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<Result<_, _>>()?;
        if !ids.is_empty() {
            knows_forget_sessions_in(&tx, &ids).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Logs the person's choice for `items` (kind, ref) that `agent` holds
    /// outside its rule, and marks them, in one transaction: kept, done at
    /// once; or being deleted, until the agent's hub has deleted them
    /// ([`Self::knows_deleted`]). Answers the log row's id.
    pub async fn knows_choice(
        &self,
        agent: &str,
        delete: bool,
        items: &[(String, String)],
        detail: &Value,
        now: DateTime<Utc>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let what = if delete { "delete" } else { "keep" };
        let done: Option<DateTime<Utc>> = (!delete).then_some(now);
        tx.execute(
            "INSERT INTO knows_log (id, at, agent, what, detail, done_at) VALUES ($1, $2, $3, $4, $5, $6)",
            &[&id, &now, &agent, &what, detail, &done],
        )
        .await?;
        let (kinds, refs): (Vec<&str>, Vec<&str>) =
            items.iter().map(|(k, r)| (k.as_str(), r.as_str())).unzip();
        tx.execute(
            "UPDATE knows_sent SET \
               deleting = CASE WHEN $6 THEN $2::uuid ELSE deleting END, \
               kept_at = CASE WHEN $6 THEN kept_at ELSE $5::timestamptz END \
             FROM unnest($3::text[], $4::text[]) AS d(kind, ref) \
             WHERE knows_sent.agent = $1 AND knows_sent.kind = d.kind AND knows_sent.ref = d.ref \
               AND knows_sent.deleted_at IS NULL AND knows_sent.deleting IS NULL",
            &[&agent, &id, &kinds, &refs, &now, &delete],
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// What is being deleted from `agent`'s copies: (kind, ref) of each, for
    /// the sync never to send again until it is done.
    pub async fn knows_deleting(&self, agent: &str, log: Uuid) -> Result<Vec<(String, String)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT kind, ref FROM knows_sent WHERE agent = $1 AND deleting = $2",
                &[&agent, &log],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// The deletes still waiting on an agent's hub, oldest first.
    pub async fn knows_pending_deletes(&self) -> Result<Vec<(String, LogEntry)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT agent, id, at, what, detail, done_at FROM knows_log \
                 WHERE what = 'delete' AND done_at IS NULL ORDER BY at",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, log_entry(r, 1)?)))
            .collect()
    }

    /// The delete `log` is done: the agent's copies it covered are marked
    /// deleted, and the log row says when, in one transaction.
    pub async fn knows_deleted(&self, agent: &str, log: Uuid, now: DateTime<Utc>) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "UPDATE knows_sent SET deleted_at = $3, deleting = NULL WHERE agent = $1 AND deleting = $2",
            &[&agent, &log, &now],
        )
        .await?;
        tx.execute(
            "UPDATE knows_log SET done_at = $3 WHERE id = $1 AND agent = $2",
            &[&log, &agent, &now],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// `agent`'s log, newest first.
    pub async fn knows_log(&self, agent: &str) -> Result<Vec<LogEntry>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, at, what, detail, done_at FROM knows_log WHERE agent = $1 \
                 ORDER BY at DESC LIMIT $2",
                &[&agent, &LOG_PAGE],
            )
            .await?
            .iter()
            .map(|r| log_entry(r, 0))
            .collect()
    }

    /// `items` and `bytes` were just sent to `agent`: counted in the hour
    /// they were sent in, for the day's limit. Rows over two days old go.
    pub async fn knows_spent_add(
        &self,
        agent: &str,
        at: DateTime<Utc>,
        items: i64,
        bytes: i64,
    ) -> Result<()> {
        let hour = hour_of(at);
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO knows_spent (agent, hour, items, bytes) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (agent, hour) DO UPDATE SET items = knows_spent.items + EXCLUDED.items, \
               bytes = knows_spent.bytes + EXCLUDED.bytes",
            &[&agent, &hour, &items, &bytes],
        )
        .await?;
        tx.execute(
            "DELETE FROM knows_spent WHERE hour < $1",
            &[&(hour - chrono::Duration::hours(48))],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The bytes sent to `agent` in the 24 hours up to `now`. Counted by
    /// the hour, so an hour partly outside the day counts whole: never
    /// less than was sent.
    pub async fn knows_spent_day(&self, agent: &str, now: DateTime<Utc>) -> Result<i64> {
        let client = self.pool().get().await?;
        let since = hour_of(now - chrono::Duration::hours(24));
        let row = client
            .query_one(
                "SELECT coalesce(sum(bytes), 0)::bigint FROM knows_spent \
                 WHERE agent = $1 AND hour >= $2",
                &[&agent, &since],
            )
            .await?;
        Ok(row.try_get(0)?)
    }

    /// What was shared with `agent` since `since`, by kind: items and bytes.
    pub async fn knows_shared_since(
        &self,
        agent: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<(String, i64, i64)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT kind, count(*), coalesce(sum(bytes), 0)::bigint FROM knows_sent \
                 WHERE agent = $1 AND sent_at >= $2 GROUP BY kind ORDER BY kind",
                &[&agent, &since],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?)))
            .collect()
    }

    /// Each board's tickets' open assignments: (board, session).
    pub async fn knows_assigned(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT t.board, a.session FROM ticket_assignments a \
                 JOIN tickets t ON t.id = a.ticket WHERE a.until_at IS NULL",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// How far `kind` has been shared with `agent`.
    pub async fn knows_cursor(&self, agent: &str, kind: &str) -> Result<Value> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT cursor FROM knows_cursors WHERE agent = $1 AND kind = $2",
                &[&agent, &kind],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({})))
    }

    /// Keeps how far `kind` has been shared with `agent`.
    pub async fn knows_cursor_put(&self, agent: &str, kind: &str, cursor: &Value) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO knows_cursors (agent, kind, cursor) VALUES ($1, $2, $3) \
                 ON CONFLICT (agent, kind) DO UPDATE SET cursor = EXCLUDED.cursor",
                &[&agent, &kind, cursor],
            )
            .await?;
        Ok(())
    }
}

/// In `tx`, which forgets `ids`: each agent's copies of those sessions,
/// kept by the person's choice or not, marked for deleting under one
/// `delete` log row per agent (`why: "forgotten"`), for the delete worker
/// to take back from the agent's hub as any other delete.
pub(crate) async fn knows_forget_sessions_in(
    tx: &tokio_postgres::Transaction<'_>,
    ids: &[Uuid],
) -> Result<()> {
    tx.execute(
        "INSERT INTO knows_forgotten (session, at) SELECT unnest($1::uuid[]), now() \
         ON CONFLICT DO NOTHING",
        &[&ids],
    )
    .await?;
    tx.execute(
        "WITH hit AS ( \
           SELECT agent, kind, ref, session FROM knows_sent \
           WHERE session = ANY($1) AND deleted_at IS NULL AND deleting IS NULL FOR UPDATE), \
         logs AS ( \
           INSERT INTO knows_log (id, at, agent, what, detail) \
           SELECT gen_random_uuid(), now(), agent, 'delete', jsonb_build_object( \
             'why', 'forgotten', 'items', count(*), \
             'transcripts', count(*) FILTER (WHERE kind = 'transcript'), \
             'sessions', count(DISTINCT session)) \
           FROM hit GROUP BY agent RETURNING id, agent) \
         UPDATE knows_sent k SET deleting = logs.id FROM hit JOIN logs ON logs.agent = hit.agent \
         WHERE k.agent = hit.agent AND k.kind = hit.kind AND k.ref = hit.ref",
        &[&ids],
    )
    .await?;
    Ok(())
}

fn log_entry(r: &Row, at: usize) -> Result<LogEntry> {
    Ok(LogEntry {
        id: r.try_get(at)?,
        at: r.try_get(at + 1)?,
        what: r.try_get(at + 2)?,
        detail: r.try_get(at + 3)?,
        done_at: r.try_get(at + 4)?,
    })
}

/// The start of the hour `at` falls in.
fn hour_of(at: DateTime<Utc>) -> DateTime<Utc> {
    let secs = at.timestamp().div_euclid(3600) * 3600;
    DateTime::from_timestamp(secs, 0).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    use crate::knows::TRANSCRIPT;

    fn held(kind: &str, reference: &str, session: Option<Uuid>) -> Sent {
        Sent {
            held: Held {
                kind: kind.into(),
                reference: reference.into(),
                session,
                group: Some("patchwork".into()),
                board: None,
                boss: None,
            },
            bytes: 100,
            hash: None,
        }
    }

    #[tokio::test]
    async fn what_was_sent_is_counted_by_the_hour_for_the_day_and_outlives_a_restart() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool.clone()).await.unwrap();
            let now = DateTime::parse_from_rfc3339("2026-11-14T10:30:00Z")
                .unwrap()
                .to_utc();
            let hours = |h: i64| now - chrono::Duration::hours(h);
            store
                .knows_spent_add("hub/allan", hours(30), 5, 1_000_000)
                .await
                .unwrap();
            store
                .knows_spent_add("hub/allan", hours(3), 2, 300)
                .await
                .unwrap();
            store
                .knows_spent_add("hub/allan", hours(3), 1, 50)
                .await
                .unwrap();
            store
                .knows_spent_add("hub/bea", hours(1), 1, 7)
                .await
                .unwrap();
            // Over a day ago doesn't count; the same hour adds up; another
            // agent's is its own.
            assert_eq!(store.knows_spent_day("hub/allan", now).await.unwrap(), 350);
            assert_eq!(store.knows_spent_day("hub/bea", now).await.unwrap(), 7);
            assert_eq!(store.knows_spent_day("hub/none", now).await.unwrap(), 0);
            // The hour partly inside the day counts whole: never less than
            // was sent.
            store
                .knows_spent_add("hub/cy", hours(24), 1, 9)
                .await
                .unwrap();
            assert_eq!(store.knows_spent_day("hub/cy", now).await.unwrap(), 9);
            assert_eq!(
                store
                    .knows_spent_day("hub/cy", now + chrono::Duration::hours(1))
                    .await
                    .unwrap(),
                0
            );
            // A restart is a new store over the same database.
            drop(store);
            let again = DeskStore::with_pool(pool).await.unwrap();
            assert_eq!(again.knows_spent_day("hub/allan", now).await.unwrap(), 350);
            // Rows over two days old are dropped as new ones are written.
            let client = again.pool().get().await.unwrap();
            let old: i64 = client
                .query_one(
                    "SELECT count(*) FROM knows_spent WHERE hour < $1",
                    &[&hours(48)],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(old, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn a_rule_is_kept_revised_and_logged_and_what_was_sent_is_held_until_deleted() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let now = Utc::now();
            assert_eq!(store.knows_rule("driftwerk/allan").await.unwrap(), None);
            let r = Rule::new(&["all".into()], true, Digest::Live).unwrap();
            let k = store
                .knows_rule_put("driftwerk/allan", &r, now)
                .await
                .unwrap();
            assert_eq!((k.revision, k.rule.clone()), (1, r));
            let r2 = Rule::new(&["group:patchwork".into()], false, Digest::Off).unwrap();
            let k = store
                .knows_rule_put("driftwerk/allan", &r2, now)
                .await
                .unwrap();
            assert_eq!(k.revision, 2);
            assert_eq!(store.knows_rules().await.unwrap().len(), 1);
            let s = Uuid::from_u128(1);
            store
                .knows_sent_put(
                    "driftwerk/allan",
                    &[
                        held("account", "a1", Some(s)),
                        held(TRANSCRIPT, "x1", Some(s)),
                    ],
                    now,
                )
                .await
                .unwrap();
            // Sent again: once.
            store
                .knows_sent_put("driftwerk/allan", &[held("account", "a1", Some(s))], now)
                .await
                .unwrap();
            assert_eq!(store.knows_held("driftwerk/allan").await.unwrap().len(), 2);
            assert!(store
                .knows_held("patchwork/lattice")
                .await
                .unwrap()
                .is_empty());
            let log = store
                .knows_choice(
                    "driftwerk/allan",
                    true,
                    &[(TRANSCRIPT.into(), "x1".into())],
                    &serde_json::json!({"items": 1}),
                    now,
                )
                .await
                .unwrap();
            assert_eq!(store.knows_pending_deletes().await.unwrap().len(), 1);
            // Being deleted: no longer held for a narrowing to ask about, and
            // named for the sync to leave alone.
            assert_eq!(store.knows_held("driftwerk/allan").await.unwrap().len(), 1);
            assert_eq!(
                store.knows_deleting("driftwerk/allan", log).await.unwrap(),
                [(TRANSCRIPT.to_string(), "x1".to_string())]
            );
            // A second choice over the same rows marks none of them again.
            let again = store
                .knows_choice(
                    "driftwerk/allan",
                    true,
                    &[(TRANSCRIPT.into(), "x1".into())],
                    &serde_json::json!({"items": 1}),
                    now,
                )
                .await
                .unwrap();
            assert!(store
                .knows_deleting("driftwerk/allan", again)
                .await
                .unwrap()
                .is_empty());
            store
                .knows_deleted("driftwerk/allan", log, now)
                .await
                .unwrap();
            store
                .knows_deleted("driftwerk/allan", again, now)
                .await
                .unwrap();
            let held_now = store.knows_held("driftwerk/allan").await.unwrap();
            assert_eq!(held_now.len(), 1);
            assert_eq!(held_now[0].kind, "account");
            assert!(store.knows_pending_deletes().await.unwrap().is_empty());
            // Kept by choice: settled, until it is sent again.
            store
                .knows_choice(
                    "driftwerk/allan",
                    false,
                    &[("account".into(), "a1".into())],
                    &serde_json::json!({}),
                    now,
                )
                .await
                .unwrap();
            assert!(store
                .knows_held("driftwerk/allan")
                .await
                .unwrap()
                .is_empty());
            store
                .knows_sent_put("driftwerk/allan", &[held("account", "a1", Some(s))], now)
                .await
                .unwrap();
            assert_eq!(store.knows_held("driftwerk/allan").await.unwrap().len(), 1);
            let whats: Vec<String> = store
                .knows_log("driftwerk/allan")
                .await
                .unwrap()
                .into_iter()
                .map(|e| e.what)
                .collect();
            assert_eq!(whats.iter().filter(|w| *w == "rule").count(), 2);
            assert!(whats.contains(&"delete".to_string()) && whats.contains(&"keep".to_string()));
            let shared = store
                .knows_shared_since("driftwerk/allan", now - chrono::Duration::hours(1))
                .await
                .unwrap();
            assert_eq!(shared.len(), 2);
            store
                .knows_cursor_put("driftwerk/allan", TRANSCRIPT, &serde_json::json!({"at": 5}))
                .await
                .unwrap();
            assert_eq!(
                store
                    .knows_cursor("driftwerk/allan", TRANSCRIPT)
                    .await
                    .unwrap()["at"],
                5
            );
            assert_eq!(
                store
                    .knows_cursor("driftwerk/allan", "account")
                    .await
                    .unwrap(),
                serde_json::json!({})
            );
        })
        .await;
    }
}
