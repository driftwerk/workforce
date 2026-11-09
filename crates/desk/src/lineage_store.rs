//! Handoff's lineage in Postgres (`work_lineage`, `sessions.handed_to`):
//! which sessions are one piece of work, the brief each continuing session
//! was given, and what a brief is read from.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use uuid::Uuid;

use crate::engine::Engine;
use crate::handoff::Link;
use crate::memory::brief::Included;
use crate::memory_index_store::FullExchange;
use crate::store::DeskStore;

/// A session given work, as a handoff writes it.
#[derive(Debug, Clone, PartialEq)]
pub struct LineageWrite<'a> {
    pub session: Uuid,
    pub link: &'a Link,
    /// The engine account it runs on, by id.
    pub account: Option<Uuid>,
    pub brief: Option<&'a str>,
    pub included: &'a Included,
}

/// A session's lineage as kept.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptLineage {
    pub link: Link,
    pub account: Option<Uuid>,
    pub brief: Option<String>,
}

fn link_of(r: &tokio_postgres::Row) -> Result<(Uuid, Link)> {
    Ok((
        r.try_get("session")?,
        Link {
            work: r.try_get("work")?,
            from: r.try_get("from_session")?,
            at: r.try_get("handed_at")?,
            engine: Engine::parse(r.try_get("engine")?)?,
            group: r.try_get("group_id")?,
            included: serde_json::from_value(r.try_get("included")?).unwrap_or_default(),
        },
    ))
}

impl DeskStore {
    /// Every session given work, and every session whose work continues in
    /// another.
    pub async fn lineage(&self) -> Result<(Vec<(Uuid, Link)>, Vec<(Uuid, Uuid)>)> {
        let client = self.pool().get().await?;
        let links = client
            .query(
                "SELECT session, work, from_session, handed_at, engine, group_id, included \
                 FROM work_lineage",
                &[],
            )
            .await
            .context("reading lineage")?
            .iter()
            // One a newer hub kept that this one can't read is left out.
            .filter_map(|r| link_of(r).ok())
            .collect();
        let handed = client
            .query(
                "SELECT id, handed_to FROM sessions WHERE handed_to IS NOT NULL",
                &[],
            )
            .await
            .context("reading who work was handed to")?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok((links, handed))
    }

    /// Keeps the lineage of `w.session` with its brief, before it starts:
    /// the first thing it does is read it.
    pub async fn put_lineage(&self, w: &LineageWrite<'_>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO work_lineage (session, work, from_session, handed_at, engine, \
                   account, group_id, brief, included) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (session) DO UPDATE SET work = EXCLUDED.work, \
                   from_session = EXCLUDED.from_session, handed_at = EXCLUDED.handed_at, \
                   engine = EXCLUDED.engine, account = EXCLUDED.account, \
                   group_id = EXCLUDED.group_id, brief = EXCLUDED.brief, \
                   included = EXCLUDED.included",
                &[
                    &w.session,
                    &w.link.work,
                    &w.link.from,
                    &w.link.at,
                    &w.link.engine.as_str(),
                    &w.account,
                    &w.link.group,
                    &w.brief,
                    &serde_json::to_value(w.included)?,
                ],
            )
            .await
            .context("keeping a handoff's lineage")?;
        Ok(())
    }

    /// `session`'s lineage as kept, its brief with it.
    pub async fn lineage_row(&self, session: Uuid) -> Result<Option<KeptLineage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "SELECT session, work, from_session, handed_at, engine, group_id, included, \
                   account, brief FROM work_lineage WHERE session = $1",
                &[&session],
            )
            .await?
            .map(|r| {
                Ok(KeptLineage {
                    link: link_of(&r)?.1,
                    account: r.try_get("account")?,
                    brief: r.try_get("brief")?,
                })
            })
            .transpose()
    }

    /// Puts back `session`'s lineage as it was before a handoff that didn't
    /// start: `was`, or none.
    pub async fn restore_lineage(&self, session: Uuid, was: Option<&KeptLineage>) -> Result<()> {
        match was {
            None => {
                let client = self.pool().get().await?;
                client
                    .execute("DELETE FROM work_lineage WHERE session = $1", &[&session])
                    .await
                    .context("dropping a handoff that didn't start")?;
                Ok(())
            }
            Some(k) => {
                self.put_lineage(&LineageWrite {
                    session,
                    link: &k.link,
                    account: k.account,
                    brief: k.brief.as_deref(),
                    included: &k.link.included,
                })
                .await
            }
        }
    }

    /// Says the work of `from` continues in `to` (none: in `from` again),
    /// and, once handed back, that `to`'s no longer continues elsewhere.
    pub async fn hand_on(&self, from: Uuid, to: Option<Uuid>, back: Option<Uuid>) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "UPDATE sessions SET handed_to = $2 WHERE id = $1",
            &[&from, &to],
        )
        .await
        .context("saying where work continues")?;
        if let Some(back) = back {
            tx.execute(
                "UPDATE sessions SET handed_to = NULL WHERE id = $1",
                &[&back],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `session` is forgotten: no work goes on in it any more, and it holds
    /// none. The sessions whose work went on in it are where it is again.
    pub async fn forget_lineage(&self, session: Uuid) -> Result<Vec<Uuid>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let back: Vec<Uuid> = tx
            .query(
                "UPDATE sessions SET handed_to = NULL WHERE handed_to = $1 RETURNING id",
                &[&session],
            )
            .await
            .context("taking work back from a forgotten session")?
            .iter()
            .map(|r| r.get(0))
            .collect();
        tx.execute("DELETE FROM work_lineage WHERE session = $1", &[&session])
            .await
            .context("forgetting a session's lineage")?;
        tx.commit().await?;
        Ok(back)
    }

    /// The brief `session` was given (none once forgotten), and the group
    /// it was given in; none when it was handed no work.
    pub async fn lineage_brief(
        &self,
        session: Uuid,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "SELECT brief, group_id FROM work_lineage WHERE session = $1",
                &[&session],
            )
            .await?
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .transpose()
    }

    /// `session`'s exchanges a brief may be written from, oldest first: at
    /// most the latest `limit`, from `since` when a hand back asks only
    /// what is new; and how many of its exchanges there are in all.
    pub async fn brief_exchanges(
        &self,
        session: Uuid,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<(Vec<FullExchange>, i64)> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM (SELECT id, session, group_id, project, cardinality(flagged) > 0, \
                   seq, at, ended, asker, ask, answer, steps, files, commands, errors, \
                   compaction, redacted \
                 FROM memory_exchanges WHERE session = $1 \
                   AND ($2::timestamptz IS NULL OR at >= $2) \
                 ORDER BY seq DESC LIMIT $3) x ORDER BY seq",
                &[&session, &since, &limit],
            )
            .await
            .context("reading a brief's exchanges")?;
        let all: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1 \
                   AND ($2::timestamptz IS NULL OR at >= $2)",
                &[&session, &since],
            )
            .await?
            .try_get(0)?;
        let exchanges = rows
            .iter()
            .map(|r| {
                Ok(FullExchange {
                    id: r.try_get(0)?,
                    session: r.try_get(1)?,
                    group: r.try_get(2)?,
                    project: r.try_get(3)?,
                    flagged: r.try_get(4)?,
                    exchange: crate::memory_store::KeptExchange {
                        seq: r.try_get(5)?,
                        at: r.try_get(6)?,
                        ended: r.try_get(7)?,
                        asker: crate::memory::record::Asker::parse(r.try_get(8)?)?,
                        ask: r.try_get(9)?,
                        answer: r.try_get(10)?,
                        steps: serde_json::from_value(r.try_get(11)?).unwrap_or_default(),
                        files: r.try_get(12)?,
                        commands: r.try_get(13)?,
                        errors: r.try_get(14)?,
                        compaction: r.try_get(15)?,
                        redacted: r.try_get(16)?,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((exchanges, all))
    }
}
