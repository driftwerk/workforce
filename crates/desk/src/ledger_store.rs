//! The ledger in Postgres: its rows, kept once by `(engine, key)`, and the
//! cursor of every record read, written together so a re-read never counts
//! a call twice.

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::engine::Engine;
use crate::ledger::{Billing, Cell, Priced, Spend, Tokens};
use crate::model::SessionRecord;
use crate::scope::{seen_sql, Seen, SeenParams};
use crate::store::DeskStore;

/// How far one of a session's records has been read: by the ledger, and
/// by the harvest of its conversation, each from its own offset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cursor {
    pub read_to: u64,
    /// What its reader carries from line to line.
    pub seen: Value,
    /// How far its conversation has been harvested.
    pub harvested_to: u64,
    /// What the harvest carries from line to line (a
    /// [`Harvested`](crate::memory::record::Harvested)).
    pub harvest: Value,
}

/// A call as the ledger keeps it: priced, and stamped with how it was paid
/// and by which account, where Desk knows.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub spend: Spend,
    pub billing: Billing,
    pub account: Option<String>,
}

/// Which rows a sum or a page is of.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub engine: Option<Engine>,
    pub account: Option<String>,
    pub session: Option<Uuid>,
    /// Only the rows an overseer of this scope sees, by the group each was
    /// kept in; every row for none (the person).
    pub seen: Option<Seen>,
}

impl Filter {
    /// The scope's group and the groups with no boss, which [`seen_sql`] takes,
    /// and whether every row is read (there is no scope to keep to).
    fn seen_params(&self) -> (SeenParams, bool) {
        match &self.seen {
            Some(s) => (s.params(), false),
            None => (SeenParams::default(), true),
        }
    }
}

/// One kept call, as the Ledger view lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerRow {
    pub session: Uuid,
    pub key: String,
    pub at: DateTime<Utc>,
    pub engine: Engine,
    pub model: Option<String>,
    pub tokens: Tokens,
    pub usd: Option<f64>,
    pub priced: Priced,
    pub billing: Billing,
    pub account: Option<String>,
    /// The board's ticket the session was working on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<Uuid>,
    /// The sidebar group the session was in when the call was kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Where a page of rows ends: the next page starts after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct After {
    pub at: DateTime<Utc>,
    pub engine: Engine,
    pub key: String,
}

impl After {
    /// As a URL carries it.
    pub fn encode(&self) -> String {
        format!(
            "{}~{}~{}",
            self.at.timestamp_micros(),
            self.engine.as_str(),
            self.key
        )
    }

    pub fn decode(s: &str) -> Option<Self> {
        let mut parts = s.splitn(3, '~');
        let at = DateTime::from_timestamp_micros(parts.next()?.parse().ok()?)?;
        let engine = Engine::parse(parts.next()?).ok()?;
        let key = parts.next()?.to_string();
        Some(Self { at, engine, key })
    }
}

/// Most rows one page of the ledger holds.
pub const PAGE: i64 = 200;

fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn tokens_of(row: &tokio_postgres::Row, at: usize) -> Result<Tokens> {
    let n = |i: usize| -> Result<u64> {
        Ok(u64::try_from(row.try_get::<_, i64>(at + i)?).unwrap_or(0))
    };
    Ok(Tokens {
        input: n(0)?,
        output: n(1)?,
        cache_read: n(2)?,
        cache_write: n(3)?,
        reasoning: n(4)?,
    })
}

/// An engine's sign-in as the ledger keeps it across restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptSignIn {
    pub engine: Engine,
    pub via: crate::engine::Via,
    pub who: Option<String>,
    pub since: DateTime<Utc>,
}

impl DeskStore {
    /// Keeps `s` as its engine's sign-in now: the same sign-in as kept
    /// keeps the earlier `since`; another replaces it. And notes its way of
    /// signing in as one the engine has been seen with.
    pub async fn keep_sign_in(&self, s: &KeptSignIn) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO ledger_sign_ins (engine, via, who, since) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (engine) DO UPDATE SET \
                   since = CASE WHEN ledger_sign_ins.via = EXCLUDED.via \
                                 AND ledger_sign_ins.who IS NOT DISTINCT FROM EXCLUDED.who \
                            THEN LEAST(ledger_sign_ins.since, EXCLUDED.since) \
                            ELSE EXCLUDED.since END, \
                   via = EXCLUDED.via, who = EXCLUDED.who",
                &[&s.engine.as_str(), &s.via.as_str(), &s.who, &s.since],
            )
            .await
            .context("keeping an engine's sign-in")?;
        client
            .execute(
                "INSERT INTO ledger_billings_seen (engine, via, since) VALUES ($1, $2, $3) \
                 ON CONFLICT (engine, via) DO UPDATE SET since = LEAST(ledger_billings_seen.since, EXCLUDED.since)",
                &[&s.engine.as_str(), &s.via.as_str(), &s.since],
            )
            .await
            .context("noting how an engine was signed in")?;
        Ok(())
    }

    /// Each engine's sign-in as last kept.
    pub async fn kept_sign_ins(&self) -> Result<Vec<KeptSignIn>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT engine, via, who, since FROM ledger_sign_ins", &[])
            .await
            .context("reading the engines' sign-ins")?;
        rows.iter()
            .filter_map(|r| {
                let engine: String = r.get(0);
                let via: String = r.get(1);
                Some(Ok(KeptSignIn {
                    engine: Engine::parse(&engine).ok()?,
                    via: crate::engine::Via::parse(&via).ok()?,
                    who: r.get(2),
                    since: r.get(3),
                }))
            })
            .collect()
    }

    /// Every way each engine has been seen signed in.
    pub async fn billings_seen(
        &self,
    ) -> Result<std::collections::BTreeMap<Engine, std::collections::BTreeSet<crate::engine::Via>>>
    {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT engine, via FROM ledger_billings_seen", &[])
            .await
            .context("reading how the engines were signed in")?;
        let mut seen: std::collections::BTreeMap<_, std::collections::BTreeSet<_>> =
            Default::default();
        for r in &rows {
            let (engine, via): (String, String) = (r.get(0), r.get(1));
            if let (Ok(e), Ok(v)) = (Engine::parse(&engine), crate::engine::Via::parse(&via)) {
                seen.entry(e).or_default().insert(v);
            }
        }
        Ok(seen)
    }

    /// How far `source` of `session` has been read; `None` before it was.
    pub async fn ledger_cursor(&self, session: Uuid, source: &str) -> Result<Option<Cursor>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT read_to, seen, harvested_to, harvest FROM ledger_cursors \
                 WHERE session = $1 AND source = $2",
                &[&session, &source],
            )
            .await?;
        row.map(|r| {
            Ok(Cursor {
                read_to: u64::try_from(r.try_get::<_, i64>(0)?).unwrap_or(0),
                seen: r.try_get(1)?,
                harvested_to: u64::try_from(r.try_get::<_, i64>(2)?).unwrap_or(0),
                harvest: r.try_get(3)?,
            })
        })
        .transpose()
    }

    /// Keeps `rows` of `session` and moves `source`'s cursor to `to`, as one
    /// transaction: a crash between the two can't count a call twice or
    /// skip one. A call kept already (the same key, from this session or a
    /// fork of it) is kept once, by the session that had it first; a later
    /// reading that knows more of it (its model, its price, a streamed
    /// call's final counts, its account) completes it. How many rows were
    /// added or completed. A new row is stamped with the ticket the session
    /// worked on when the call was made (`ticket_assignments`), however
    /// late its record is read, and with the sidebar group the session is
    /// in as it is kept; a completed row keeps its stamps.
    pub async fn ledger_write(
        &self,
        session: Uuid,
        engine: Engine,
        source: Option<(&str, &Cursor)>,
        rows: &[Kept],
    ) -> Result<u64> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let changed = ledger_write_in(&tx, session, engine, rows).await?;
        if let Some((source, cursor)) = source {
            tx.execute(
                "INSERT INTO ledger_cursors (session, source, read_to, seen, updated_at) \
                 VALUES ($1, $2, $3, $4, now()) ON CONFLICT (session, source) DO UPDATE \
                 SET read_to = $3, seen = $4, updated_at = now()",
                &[&session, &source, &count(cursor.read_to), &cursor.seen],
            )
            .await
            .context("moving a ledger cursor")?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// The ledger between `from` and `to`, summed by session, model,
    /// account, billing and day in `tz` (an IANA name).
    pub async fn ledger_cells(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        tz: &str,
        filter: &Filter,
    ) -> Result<Vec<Cell>> {
        let client = self.pool().get().await?;
        let (p, all) = filter.seen_params();
        let sql = format!(
            "SELECT session, engine, model, account, billing, \
               (at AT TIME ZONE $3)::date AS day, \
               SUM(input)::int8, SUM(output)::int8, SUM(cache_read)::int8, \
               SUM(cache_write)::int8, SUM(reasoning)::int8, \
               COALESCE(SUM(usd), 0)::float8, COUNT(*)::int8, \
               (COUNT(*) FILTER (WHERE usd IS NULL OR priced = 'estimated'))::int8, group_id \
             FROM ledger WHERE at >= $1 AND at < $2 \
               AND ($4::text IS NULL OR engine = $4) \
               AND ($5::text IS NULL OR account = $5) \
               AND ($6::uuid IS NULL OR session = $6) \
               AND ($11::bool OR {}) \
             GROUP BY 1, 2, 3, 4, 5, 6, 15",
            seen_sql("group_id", "at", 7)
        );
        let rows = client
            .query(
                &sql,
                &[
                    &from,
                    &to,
                    &tz,
                    &filter.engine.map(Engine::as_str),
                    &filter.account,
                    &filter.session,
                    &p.group,
                    &p.open,
                    &p.groups,
                    &p.since,
                    &all,
                ],
            )
            .await
            .context("summing the ledger")?;
        rows.iter()
            .map(|r| {
                Ok(Cell {
                    session: r.try_get(0)?,
                    group: r.try_get(14)?,
                    engine: Engine::parse(r.try_get(1)?)?,
                    model: r.try_get(2)?,
                    account: r.try_get(3)?,
                    billing: Billing::parse(r.try_get(4)?)?,
                    day: r.try_get::<_, NaiveDate>(5)?,
                    tokens: tokens_of(r, 6)?,
                    usd: r.try_get(11)?,
                    calls: u64::try_from(r.try_get::<_, i64>(12)?).unwrap_or(0),
                    unpriced: u64::try_from(r.try_get::<_, i64>(13)?).unwrap_or(0),
                })
            })
            .collect()
    }

    /// The kept calls between `from` and `to`, newest first, a page at a
    /// time from `after`.
    pub async fn ledger_rows(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: &Filter,
        after: Option<&After>,
    ) -> Result<Vec<LedgerRow>> {
        let client = self.pool().get().await?;
        let (p, all) = filter.seen_params();
        let sql = format!(
            "SELECT session, key, at, engine, model, input, output, cache_read, \
               cache_write, reasoning, usd, priced, billing, account, ticket, group_id \
             FROM ledger WHERE at >= $1 AND at < $2 \
               AND ($3::text IS NULL OR engine = $3) \
               AND ($4::text IS NULL OR account = $4) \
               AND ($5::uuid IS NULL OR session = $5) \
               AND ($6::timestamptz IS NULL OR (at, engine, key) < ($6, $7, $8)) \
               AND ($14::bool OR {}) \
             ORDER BY at DESC, engine DESC, key DESC LIMIT $9",
            seen_sql("group_id", "at", 10)
        );
        let rows = client
            .query(
                &sql,
                &[
                    &from,
                    &to,
                    &filter.engine.map(Engine::as_str),
                    &filter.account,
                    &filter.session,
                    &after.map(|a| a.at),
                    &after.map(|a| a.engine.as_str()),
                    &after.map(|a| a.key.as_str()),
                    &PAGE,
                    &p.group,
                    &p.open,
                    &p.groups,
                    &p.since,
                    &all,
                ],
            )
            .await
            .context("reading the ledger")?;
        rows.iter()
            .map(|r| {
                Ok(LedgerRow {
                    session: r.try_get(0)?,
                    key: r.try_get(1)?,
                    at: r.try_get(2)?,
                    engine: Engine::parse(r.try_get(3)?)?,
                    model: r.try_get(4)?,
                    tokens: tokens_of(r, 5)?,
                    usd: r.try_get(10)?,
                    priced: Priced::parse(r.try_get(11)?)?,
                    billing: Billing::parse(r.try_get(12)?)?,
                    account: r.try_get(13)?,
                    ticket: r.try_get(14)?,
                    group: r.try_get(15)?,
                })
            })
            .collect()
    }

    /// Every session the registry has, closed ones too, oldest first: whose
    /// records the ledger reads back.
    /// How many ledger rows `session`'s calls (or, with `ticket`, the
    /// ticket's) are, and what they cost.
    pub async fn ledger_total(
        &self,
        session: Option<Uuid>,
        ticket: Option<Uuid>,
    ) -> Result<(u64, f64)> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*)::int8, COALESCE(SUM(usd), 0)::float8 FROM ledger \
                 WHERE ($1::uuid IS NULL OR session = $1) AND ($2::uuid IS NULL OR ticket = $2) \
                   AND ($1::uuid IS NOT NULL OR $2::uuid IS NOT NULL)",
                &[&session, &ticket],
            )
            .await
            .context("summing an item's ledger")?;
        Ok((
            u64::try_from(row.try_get::<_, i64>(0)?).unwrap_or(0),
            row.try_get(1)?,
        ))
    }

    pub async fn every_session(&self) -> Result<Vec<SessionRecord>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT * FROM sessions ORDER BY created_at, id", &[])
            .await?;
        rows.iter().map(crate::store::record).collect()
    }

    /// The sessions of `ids` the registry still has, closed ones too.
    pub async fn sessions_of(&self, ids: &[Uuid]) -> Result<Vec<SessionRecord>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT * FROM sessions WHERE id = ANY($1)", &[&ids])
            .await?;
        rows.iter().map(crate::store::record).collect()
    }
}

/// [`DeskStore::ledger_write`]'s rows within `tx`: how many were added or
/// completed.
pub(crate) async fn ledger_write_in(
    tx: &tokio_postgres::Transaction<'_>,
    session: Uuid,
    engine: Engine,
    rows: &[Kept],
) -> Result<u64> {
    let mut changed = 0;
    if !rows.is_empty() {
        let col = |f: &dyn Fn(&Kept) -> i64| rows.iter().map(f).collect::<Vec<i64>>();
        let keys: Vec<&str> = rows.iter().map(|r| r.spend.key.as_str()).collect();
        let ats: Vec<DateTime<Utc>> = rows.iter().map(|r| r.spend.at).collect();
        let models: Vec<Option<&str>> = rows.iter().map(|r| r.spend.model.as_deref()).collect();
        let usds: Vec<Option<f64>> = rows.iter().map(|r| r.spend.usd).collect();
        let priced: Vec<&str> = rows.iter().map(|r| r.spend.priced.as_str()).collect();
        let billing: Vec<&str> = rows.iter().map(|r| r.billing.as_str()).collect();
        let accounts: Vec<Option<&str>> = rows.iter().map(|r| r.account.as_deref()).collect();
        // Each row is the group's the session was in when it was made, so a
        // call read after a move stays where it happened.
        let group = crate::store::group_at_sql("$2::uuid", "a");
        let sql = format!(
            "INSERT INTO ledger (engine, key, session, at, model, input, output, \
                     cache_read, cache_write, reasoning, usd, priced, billing, account, ticket, \
                     group_id) \
                     SELECT $1, k, $2, a, m, i, o, cr, cw, r, u, p, b, acc, ( \
                       SELECT w.ticket FROM ticket_assignments w \
                       WHERE w.session = $2::uuid AND w.from_at <= a \
                         AND (w.until_at IS NULL OR a < w.until_at) \
                       ORDER BY w.from_at DESC LIMIT 1), {group} FROM UNNEST( \
                       $3::text[], $4::timestamptz[], $5::text[], $6::int8[], $7::int8[], \
                       $8::int8[], $9::int8[], $10::int8[], $11::float8[], $12::text[], \
                       $13::text[], $14::text[]) AS t(k, a, m, i, o, cr, cw, r, u, p, b, acc) \
                     ON CONFLICT (engine, key) DO UPDATE SET \
                       model = COALESCE(EXCLUDED.model, ledger.model), \
                       input = GREATEST(ledger.input, EXCLUDED.input), \
                       output = GREATEST(ledger.output, EXCLUDED.output), \
                       cache_read = GREATEST(ledger.cache_read, EXCLUDED.cache_read), \
                       cache_write = GREATEST(ledger.cache_write, EXCLUDED.cache_write), \
                       reasoning = GREATEST(ledger.reasoning, EXCLUDED.reasoning), \
                       usd = COALESCE(EXCLUDED.usd, ledger.usd), \
                       priced = CASE WHEN EXCLUDED.usd IS NULL THEN ledger.priced \
                                ELSE EXCLUDED.priced END, \
                       billing = CASE WHEN ledger.billing = 'unknown' THEN EXCLUDED.billing \
                                 ELSE ledger.billing END, \
                       account = COALESCE(ledger.account, EXCLUDED.account) \
                     WHERE (ledger.model IS NULL AND EXCLUDED.model IS NOT NULL) \
                        OR (ledger.usd IS NULL AND EXCLUDED.usd IS NOT NULL) \
                        OR EXCLUDED.input > ledger.input OR EXCLUDED.output > ledger.output \
                        OR EXCLUDED.cache_read > ledger.cache_read \
                        OR EXCLUDED.cache_write > ledger.cache_write \
                        OR (ledger.billing = 'unknown' AND EXCLUDED.billing <> 'unknown') \
                        OR (ledger.account IS NULL AND EXCLUDED.account IS NOT NULL)"
        );
        changed = tx
            .execute(
                sql.as_str(),
                &[
                    &engine.as_str(),
                    &session,
                    &keys,
                    &ats,
                    &models,
                    &col(&|r| count(r.spend.tokens.input)),
                    &col(&|r| count(r.spend.tokens.output)),
                    &col(&|r| count(r.spend.tokens.cache_read)),
                    &col(&|r| count(r.spend.tokens.cache_write)),
                    &col(&|r| count(r.spend.tokens.reasoning)),
                    &usds,
                    &priced,
                    &billing,
                    &accounts,
                ],
            )
            .await
            .context("keeping ledger rows")?;
    }
    Ok(changed)
}
