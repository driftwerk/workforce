//! Session memory's notes in Postgres: the durable queue of sessions whose
//! new exchanges are to be read for decisions, facts and to-dos, the calls
//! made each day, what derivation writes, and the person's own changes. A
//! worker claims a due session with `FOR UPDATE SKIP LOCKED` and a lease,
//! so a restart picks the queue up where it was; how far a session was read
//! moves only when what was noted from it is written.

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::Engine;
use crate::memory::derive::{Applied, InputItem, ItemKind, ItemState, NewItem};
use crate::memory::record::Asker;
use crate::memory_store::{group_key, PAUSED};
use crate::store::DeskStore;

/// How long after a session was last read for notes it may be read again
/// as its turns end: at most every ten minutes.
pub const DEBOUNCE_MINUTES: i32 = 10;

/// `memory_derive.due_at` as a turn's end sets it, in a transaction that
/// wrote closed exchanges of session `$1`: due at once when it was last
/// read [`DEBOUNCE_MINUTES`] or more ago, else when that is up; never later
/// than it was already due. A session whose reading failed is tried again.
pub(crate) fn arm_sql() -> String {
    format!(
        "INSERT INTO memory_derive (session, due_at) VALUES ($1, now()) \
         ON CONFLICT (session) DO UPDATE SET \
           due_at = LEAST(COALESCE(memory_derive.due_at, 'infinity'::timestamptz), \
             GREATEST(now(), COALESCE(memory_derive.derived_at, '-infinity'::timestamptz) \
               + make_interval(mins => {DEBOUNCE_MINUTES}))), \
           tries = CASE WHEN memory_derive.failed IS NULL THEN memory_derive.tries ELSE 0 END, \
           failed = NULL"
    )
}

/// A session claimed from the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub session: Uuid,
    /// The last exchange read for notes, by `seq`.
    pub derived_to: i32,
    pub tries: i32,
    /// What `due_at` was set to as it was claimed: while it still is, the
    /// claim holds, and a turn's end meanwhile changes it.
    pub lease: DateTime<Utc>,
}

/// An exchange after a session's `derived_to`, as derivation reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Unread {
    pub id: Uuid,
    pub seq: i32,
    pub at: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
    pub engine: Engine,
    pub group: Option<String>,
    pub project: String,
    pub asker: Asker,
    pub ask: String,
    pub answer: String,
    pub steps: Vec<crate::memory::record::Step>,
    /// It reads as instructions to a model: never read for notes.
    pub flagged: bool,
}

/// What derivation of a session is checked against beside the install.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeriveFacts {
    /// `memory.paused`.
    pub paused: bool,
    /// The session's own switch is off.
    pub session_off: bool,
    /// The group's `memory.group.<id>` says `derive = true`; ungrouped
    /// work is always so.
    pub group_on: bool,
}

/// Where a session's queue row stands, for its panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Queued {
    pub derived_to: i32,
    pub due_at: Option<DateTime<Utc>>,
    pub failed: Option<String>,
    /// Closed exchanges after `derived_to`, not flagged.
    pub unread: i64,
}

/// A note as the panel and the person's changes read it.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub id: Uuid,
    pub session: Uuid,
    pub group: Option<String>,
    pub project: String,
    pub kind: ItemKind,
    pub text: String,
    pub state: ItemState,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub by: String,
    pub confirmed: bool,
    pub sources: Vec<Uuid>,
    /// Some exchange it cites is still kept; none is once every one was
    /// swept.
    pub source_kept: bool,
}

/// What one derivation writes.
#[derive(Debug, Clone)]
pub struct Apply<'a> {
    pub claim: &'a Claim,
    /// The last exchange this read covers.
    pub to: i32,
    /// More closed exchanges were waiting past it: due again at once.
    pub backlog: bool,
    pub group: Option<&'a str>,
    /// `model:<name>`.
    pub by: &'a str,
    /// Each addition's id and project, beside [`Applied::add`].
    pub ids: &'a [(Uuid, String)],
    pub applied: &'a Applied,
}

const ITEM_COLUMNS: &str = "i.id, i.session, i.group_id, i.project, i.kind, i.text, i.state, \
    i.valid_from, i.valid_to, i.by, i.confirmed, i.sources, \
    EXISTS (SELECT 1 FROM memory_exchanges x WHERE x.id = ANY(i.sources))";

fn item_of(r: &tokio_postgres::Row) -> Result<ItemRow> {
    let kind: String = r.try_get(4)?;
    let state: String = r.try_get(6)?;
    Ok(ItemRow {
        id: r.try_get(0)?,
        session: r.try_get(1)?,
        group: r.try_get(2)?,
        project: r.try_get(3)?,
        kind: ItemKind::parse(&kind).ok_or_else(|| eyre::eyre!("a note of no known kind"))?,
        text: r.try_get(5)?,
        state: ItemState::parse(&state).unwrap_or(ItemState::Held),
        valid_from: r.try_get(7)?,
        valid_to: r.try_get(8)?,
        by: r.try_get(9)?,
        confirmed: r.try_get(10)?,
        sources: r.try_get(11)?,
        source_kept: r.try_get(12)?,
    })
}

impl DeskStore {
    /// How many calls the model made to note decisions on `day`.
    pub async fn derive_calls(&self, day: NaiveDate) -> Result<i64> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT calls FROM memory_derive_days WHERE day = $1",
                &[&day],
            )
            .await?
            .map(|r| r.try_get::<_, i32>(0))
            .transpose()?
            .map_or(0, i64::from))
    }

    /// Counts a call on `day`, unless `cap` are made already. Whether it
    /// may be made.
    pub async fn derive_take_call(&self, day: NaiveDate, cap: u32) -> Result<bool> {
        if cap == 0 {
            return Ok(false);
        }
        let cap = i32::try_from(cap).unwrap_or(i32::MAX);
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "INSERT INTO memory_derive_days (day, calls) VALUES ($1, 1) \
                 ON CONFLICT (day) DO UPDATE SET calls = memory_derive_days.calls + 1 \
                 WHERE memory_derive_days.calls < $2 RETURNING calls",
                &[&day, &cap],
            )
            .await?
            .is_some())
    }

    /// Gives back a call counted on `day` that no model took.
    pub async fn derive_untake_call(&self, day: NaiveDate) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE memory_derive_days SET calls = GREATEST(calls - 1, 0) WHERE day = $1",
                &[&day],
            )
            .await?;
        Ok(())
    }

    /// Claims the session due longest, for `lease_secs`: until then no
    /// other claim takes it, and once it is past (the hub stopped
    /// mid-call) it is due again.
    pub async fn derive_claim(&self, lease_secs: f64) -> Result<Option<Claim>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "UPDATE memory_derive d SET due_at = now() + make_interval(secs => $1) \
                 FROM (SELECT session FROM memory_derive WHERE due_at <= now() \
                   ORDER BY due_at, session LIMIT 1 FOR UPDATE SKIP LOCKED) c \
                 WHERE d.session = c.session \
                 RETURNING d.session, d.derived_to, d.tries, d.due_at",
                &[&lease_secs],
            )
            .await
            .context("claiming a session to note decisions from")?
            .map(|r| {
                Ok(Claim {
                    session: r.try_get(0)?,
                    derived_to: r.try_get(1)?,
                    tries: r.try_get(2)?,
                    lease: r.try_get(3)?,
                })
            })
            .transpose()
    }

    /// Lets `claim` go, due again at `due` (none: not until a turn's end or
    /// the panel asks), unless a turn's end changed it meanwhile.
    pub async fn derive_release(&self, claim: &Claim, due: Option<DateTime<Utc>>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE memory_derive SET due_at = $3 WHERE session = $1 AND due_at = $2",
                &[&claim.session, &claim.lease, &due],
            )
            .await?;
        Ok(())
    }

    /// Notes a failed reading of `claim`: tried again after `retry`, or
    /// (`gave_up`) not until the session says more, with `why` said.
    /// `skip_to`: how far it was read regardless, when what it read can't
    /// be noted from however often it is tried.
    pub async fn derive_failed(
        &self,
        claim: &Claim,
        retry: std::time::Duration,
        gave_up: bool,
        why: &str,
        skip_to: Option<i32>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        let secs = retry.as_secs_f64();
        client
            .execute(
                "UPDATE memory_derive SET tries = CASE WHEN $3 THEN 0 ELSE tries + 1 END, \
                   failed = CASE WHEN $3 THEN $4 END, \
                   derived_to = COALESCE($5, derived_to), \
                   due_at = CASE WHEN due_at IS DISTINCT FROM $2 THEN due_at \
                     WHEN $3 THEN NULL ELSE now() + make_interval(secs => $6) END \
                 WHERE session = $1",
                &[
                    &claim.session,
                    &claim.lease,
                    &gave_up,
                    &why,
                    &skip_to,
                    &secs,
                ],
            )
            .await?;
        Ok(())
    }

    /// Makes session `session`'s reading for notes due now, when it has
    /// exchanges not yet read. Whether it did.
    pub async fn derive_due_now(&self, session: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "INSERT INTO memory_derive (session, due_at) \
                 SELECT $1, now() WHERE EXISTS (SELECT 1 FROM memory_exchanges x \
                   WHERE x.session = $1 AND x.ended IS NOT NULL AND x.seq > COALESCE( \
                     (SELECT derived_to FROM memory_derive WHERE session = $1), 0)) \
                 ON CONFLICT (session) DO UPDATE SET \
                   due_at = LEAST(COALESCE(memory_derive.due_at, 'infinity'::timestamptz), now()), \
                   tries = 0, failed = NULL",
                &[&session],
            )
            .await?
            > 0)
    }

    /// `session`'s exchanges after `after`, in order, at most `limit`.
    pub async fn derive_unread(
        &self,
        session: Uuid,
        after: i32,
        limit: i64,
    ) -> Result<Vec<Unread>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, seq, at, ended, engine, group_id, project, asker, ask, answer, steps, \
                   cardinality(flagged) > 0 \
                 FROM memory_exchanges WHERE session = $1 AND seq > $2 ORDER BY seq LIMIT $3",
                &[&session, &after, &limit],
            )
            .await
            .context("reading exchanges to note decisions from")?
            .iter()
            .map(|r| {
                Ok(Unread {
                    id: r.try_get(0)?,
                    seq: r.try_get(1)?,
                    at: r.try_get(2)?,
                    ended: r.try_get(3)?,
                    engine: Engine::parse(r.try_get(4)?)?,
                    group: r.try_get(5)?,
                    project: r.try_get(6)?,
                    asker: Asker::parse(r.try_get(7)?)?,
                    ask: r.try_get(8)?,
                    answer: r.try_get(9)?,
                    steps: serde_json::from_value(r.try_get(10)?).unwrap_or_default(),
                    flagged: r.try_get(11)?,
                })
            })
            .collect()
    }

    /// The person's switches derivation of `session` in `group` is checked
    /// against.
    pub async fn derive_facts(&self, session: Uuid, group: Option<&str>) -> Result<DeriveFacts> {
        let client = self.pool().get().await?;
        let session_off = client
            .query_opt("SELECT memory FROM sessions WHERE id = $1", &[&session])
            .await?
            .map(|r| r.try_get::<_, String>(0))
            .transpose()?
            .is_some_and(|m| m == "off");
        let keys: Vec<String> = [Some(PAUSED.to_string()), group.map(group_key)]
            .into_iter()
            .flatten()
            .collect();
        let mut facts = DeriveFacts {
            session_off,
            group_on: group.is_none(),
            ..Default::default()
        };
        for row in client
            .query(
                "SELECT key, value FROM settings WHERE key = ANY($1)",
                &[&keys],
            )
            .await?
        {
            let (key, value): (String, Value) = (row.try_get(0)?, row.try_get(1)?);
            if key == PAUSED {
                facts.paused = value.as_bool().unwrap_or(false);
            } else {
                facts.group_on = value.get("derive").and_then(Value::as_bool) == Some(true);
            }
        }
        Ok(facts)
    }

    /// `session`'s notes still held in `group`, oldest first: what
    /// derivation may close or replace.
    pub async fn held_items(&self, session: Uuid, group: Option<&str>) -> Result<Vec<InputItem>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, kind, text, state, valid_from, confirmed OR by = 'person' \
                 FROM memory_items WHERE session = $1 AND group_id IS NOT DISTINCT FROM $2 \
                   AND valid_to IS NULL ORDER BY valid_from, id LIMIT 200",
                &[&session, &group],
            )
            .await?
            .iter()
            .filter_map(|r| {
                let kind = ItemKind::parse(r.get(1))?;
                Some(Ok(InputItem {
                    id: r.get(0),
                    kind,
                    text: r.get(2),
                    state: ItemState::parse(r.get(3)).unwrap_or(ItemState::Held),
                    valid_from: r.get(4),
                    confirmed: r.get(5),
                }))
            })
            .collect()
    }

    /// Writes what derivation noted, and moves how far `claim`'s session
    /// was read to `to`, in one transaction: only while it was read from
    /// where the claim says (a session forgotten meanwhile, or read by
    /// another, gets nothing). Whether it was written.
    pub async fn derive_apply(&self, a: &Apply<'_>) -> Result<bool> {
        let session = a.claim.session;
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // As forgetting locks: the session, then its queue row, then its
        // notes.
        tx.execute(
            "SELECT 1 FROM sessions WHERE id = $1 FOR SHARE",
            &[&session],
        )
        .await?;
        let read = tx
            .query_opt(
                "SELECT derived_to FROM memory_derive WHERE session = $1 FOR UPDATE",
                &[&session],
            )
            .await?
            .map(|r| r.try_get::<_, i32>(0))
            .transpose()?;
        if read != Some(a.claim.derived_to) {
            tx.rollback().await?;
            return Ok(false);
        }
        for ((id, project), item) in a.ids.iter().zip(&a.applied.add) {
            insert_item(&tx, *id, session, a.group, project, item, a.by).await?;
        }
        for s in &a.applied.supersede {
            let Some((by, _)) = a.ids.get(s.by) else {
                continue;
            };
            // A note the person changed meanwhile stays as they left it.
            let replaced = tx
                .execute(
                    "UPDATE memory_items SET valid_to = $3, superseded_by = $4 \
                     WHERE id = $1 AND session = $2 AND valid_to IS NULL AND NOT confirmed \
                       AND by <> 'person'",
                    &[&s.id, &session, &s.valid_to, by],
                )
                .await
                .context("replacing a note")?;
            // What it replaced leaves the search index; Postgres keeps it.
            if replaced > 0 {
                crate::memory_store::queue_purge(
                    &tx,
                    a.group,
                    &crate::memory::passage::Purge::Refs { refs: vec![s.id] },
                )
                .await?;
            }
        }
        for c in &a.applied.close {
            tx.execute(
                "UPDATE memory_items SET state = $3 \
                 WHERE id = $1 AND session = $2 AND kind = 'todo' AND state = 'open' \
                   AND valid_to IS NULL",
                &[&c.id, &session, &c.state.as_str()],
            )
            .await
            .context("closing a to-do")?;
        }
        tx.execute(
            &format!(
                "UPDATE memory_derive SET derived_to = $2, derived_at = now(), tries = 0, \
                   failed = NULL, due_at = CASE WHEN $4 THEN now() \
                     WHEN due_at IS DISTINCT FROM $3 \
                       THEN GREATEST(due_at, now() + make_interval(mins => {DEBOUNCE_MINUTES})) \
                     ELSE NULL END \
                 WHERE session = $1"
            ),
            &[&session, &a.to, &a.claim.lease, &a.backlog],
        )
        .await
        .context("moving how far a session was read for notes")?;
        tx.commit().await?;
        Ok(true)
    }

    /// Where `session`'s queue row stands.
    pub async fn derive_queued(&self, session: Uuid) -> Result<Queued> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT derived_to, due_at, failed FROM memory_derive WHERE session = $1",
                &[&session],
            )
            .await?;
        let (derived_to, due_at, failed) = match row {
            Some(r) => (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?),
            None => (0, None, None),
        };
        let unread: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM memory_exchanges WHERE session = $1 AND seq > $2 \
                   AND ended IS NOT NULL AND cardinality(flagged) = 0",
                &[&session, &derived_to],
            )
            .await?
            .try_get(0)?;
        Ok(Queued {
            derived_to,
            due_at,
            failed,
            unread,
        })
    }

    /// The group `session`'s latest exchange was kept in: a session Desk no
    /// longer has is read as it was last.
    pub async fn latest_group(&self, session: Uuid) -> Result<Option<String>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT group_id FROM memory_exchanges WHERE session = $1 ORDER BY seq DESC LIMIT 1",
                &[&session],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()?
            .flatten())
    }

    /// `session`'s notes still held, oldest first.
    pub async fn session_items(&self, session: Uuid) -> Result<Vec<ItemRow>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM memory_items i \
                     WHERE i.session = $1 AND i.valid_to IS NULL ORDER BY i.valid_from, i.id"
                ),
                &[&session],
            )
            .await?
            .iter()
            .map(item_of)
            .collect()
    }

    /// The note `id`.
    pub async fn memory_item(&self, id: Uuid) -> Result<Option<ItemRow>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {ITEM_COLUMNS} FROM memory_items i WHERE i.id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(item_of)
            .transpose()
    }

    /// The person confirms note `id`: it ranks above what a model noted.
    /// Whether it was still held.
    pub async fn confirm_item(&self, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE memory_items SET confirmed = true WHERE id = $1 AND valid_to IS NULL",
                &[&id],
            )
            .await?
            > 0)
    }

    /// The person corrects note `id` to `text`: a note of theirs, confirmed,
    /// replaces it, citing what it cited, so since when it was. The new
    /// note's id, if it was still held.
    pub async fn correct_item(&self, id: Uuid, text: &str) -> Result<Option<Uuid>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // As forgetting locks: the session's queue row before its notes, so
        // a correction is either seen by a forget or finds its note gone.
        tx.execute(
            "SELECT 1 FROM memory_derive WHERE session = \
               (SELECT session FROM memory_items WHERE id = $1) FOR SHARE",
            &[&id],
        )
        .await?;
        let Some(held) = tx
            .query_opt(
                "SELECT group_id FROM memory_items WHERE id = $1 AND valid_to IS NULL FOR UPDATE",
                &[&id],
            )
            .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };
        let group: Option<String> = held.try_get(0)?;
        let new = Uuid::new_v4();
        tx.execute(
            "INSERT INTO memory_items (id, session, group_id, project, kind, text, state, \
               valid_from, sources, by, confirmed, written_at) \
             SELECT $2, session, group_id, project, kind, $3, state, valid_from, sources, \
               'person', true, now() \
             FROM memory_items WHERE id = $1",
            &[&id, &new, &text],
        )
        .await
        .context("correcting a note")?;
        tx.execute(
            "UPDATE memory_items SET valid_to = now(), superseded_by = $2 WHERE id = $1",
            &[&id, &new],
        )
        .await?;
        crate::memory_store::queue_purge(
            &tx,
            group.as_deref(),
            &crate::memory::passage::Purge::Refs { refs: vec![id] },
        )
        .await?;
        tx.commit().await?;
        Ok(Some(new))
    }

    /// The person drops note `id`: it goes, and its passage is queued to
    /// go from the search index. Whether there was one.
    pub async fn drop_item(&self, id: Uuid) -> Result<bool> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "DELETE FROM memory_items WHERE id = $1 RETURNING group_id",
                &[&id],
            )
            .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };
        let group: Option<String> = row.try_get(0)?;
        crate::memory_store::queue_purge(
            &tx,
            group.as_deref(),
            &crate::memory::passage::Purge::Refs { refs: vec![id] },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}

async fn insert_item(
    tx: &tokio_postgres::Transaction<'_>,
    id: Uuid,
    session: Uuid,
    group: Option<&str>,
    project: &str,
    item: &NewItem,
    by: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO memory_items (id, session, group_id, project, kind, text, state, \
           valid_from, sources, by, written_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())",
        &[
            &id,
            &session,
            &group,
            &project,
            &item.kind.as_str(),
            &item.text,
            &item.state.as_str(),
            &item.valid_from,
            &item.sources,
            &by,
        ],
    )
    .await
    .context("keeping a note")?;
    Ok(())
}
