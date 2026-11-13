//! What each agent knows of the work, in Postgres: its rule
//! (`knows_rules`), what was shared with it (`knows_sent`), how far each
//! kind has gone (`knows_cursors`), and the person's changes and choices
//! (`knows_log`).

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

    /// What `agent` holds now: everything shared with it and not deleted.
    pub async fn knows_held(&self, agent: &str) -> Result<Vec<Held>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT kind, ref, session, group_id, board, boss FROM knows_sent \
                 WHERE agent = $1 AND deleted_at IS NULL",
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
    /// time renewed when sent again, and held again if it was deleted.
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
                "INSERT INTO knows_sent (agent, kind, ref, session, group_id, board, boss, bytes, sent_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (agent, kind, ref) DO UPDATE SET session = EXCLUDED.session, \
                   group_id = EXCLUDED.group_id, board = EXCLUDED.board, boss = EXCLUDED.boss, \
                   bytes = EXCLUDED.bytes, sent_at = EXCLUDED.sent_at, deleted_at = NULL",
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
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Logs the person's choice for what `agent` held outside its rule:
    /// `keep` is done at once; `delete` waits until the agent's hub has
    /// deleted the copies ([`Self::knows_deleted`]). Answers its id.
    pub async fn knows_choice(
        &self,
        agent: &str,
        delete: bool,
        detail: &Value,
        now: DateTime<Utc>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let client = self.pool().get().await?;
        let what = if delete { "delete" } else { "keep" };
        let done: Option<DateTime<Utc>> = (!delete).then_some(now);
        client
            .execute(
                "INSERT INTO knows_log (id, at, agent, what, detail, done_at) VALUES ($1, $2, $3, $4, $5, $6)",
                &[&id, &now, &agent, &what, detail, &done],
            )
            .await?;
        Ok(id)
    }

    /// The deletes still waiting on an agent's hub, oldest first.
    pub async fn knows_pending_deletes(&self) -> Result<Vec<(String, LogEntry)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT agent, id, at, what, detail, done_at FROM knows_log \
                 WHERE what = 'delete' AND done_at IS NULL ORDER BY at LIMIT 100",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, log_entry(r, 1)?)))
            .collect()
    }

    /// The delete `log` is done: the agent's copies of `items` (kind, ref)
    /// are marked deleted and the log row says when, in one transaction.
    pub async fn knows_deleted(
        &self,
        agent: &str,
        log: Uuid,
        items: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let (kinds, refs): (Vec<&str>, Vec<&str>) =
            items.iter().map(|(k, r)| (k.as_str(), r.as_str())).unzip();
        tx.execute(
            "UPDATE knows_sent SET deleted_at = $2 \
             FROM unnest($3::text[], $4::text[]) AS d(kind, ref) \
             WHERE knows_sent.agent = $1 AND knows_sent.kind = d.kind AND knows_sent.ref = d.ref \
               AND knows_sent.deleted_at IS NULL",
            &[&agent, &now, &kinds, &refs],
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

fn log_entry(r: &Row, at: usize) -> Result<LogEntry> {
    Ok(LogEntry {
        id: r.try_get(at)?,
        at: r.try_get(at + 1)?,
        what: r.try_get(at + 2)?,
        detail: r.try_get(at + 3)?,
        done_at: r.try_get(at + 4)?,
    })
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
        }
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
                    &serde_json::json!({"items": 1}),
                    now,
                )
                .await
                .unwrap();
            assert_eq!(store.knows_pending_deletes().await.unwrap().len(), 1);
            store
                .knows_deleted(
                    "driftwerk/allan",
                    log,
                    &[(TRANSCRIPT.into(), "x1".into())],
                    now,
                )
                .await
                .unwrap();
            let held_now = store.knows_held("driftwerk/allan").await.unwrap();
            assert_eq!(held_now.len(), 1);
            assert_eq!(held_now[0].kind, "account");
            assert!(store.knows_pending_deletes().await.unwrap().is_empty());
            store
                .knows_choice("driftwerk/allan", false, &serde_json::json!({}), now)
                .await
                .unwrap();
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
