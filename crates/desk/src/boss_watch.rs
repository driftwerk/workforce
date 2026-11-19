//! The waits a boss at Act fully starts itself (`crew.watch`), kept in
//! Postgres so a restart loses none: a pull request (until its checks are
//! done, or until it merges or closes), a CI run (until it completes) or a
//! time. The hub polls each as the person who owns it until it ends; its
//! end is told to the boss once ([`DeskStore::watches_delivered`]).
//!
//! Bounded: at most [`MAX_OPEN`] open per boss and person, each for at most
//! [`MAX_HOURS`].

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::store::DeskStore;

/// The most watches open at once for one boss and person.
pub const MAX_OPEN: usize = 20;
/// How long a watch waits by default, and at most.
pub const DEFAULT_HOURS: i64 = 24;
pub const MAX_HOURS: i64 = 72;

/// What a watch waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    /// A pull request: its checks done, or (`until_merged`) merged or closed.
    Pr,
    /// A CI run: completed.
    Run,
    /// A time: reached.
    Time,
}

impl WatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Run => "run",
            Self::Time => "time",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pr" => Some(Self::Pr),
            "run" => Some(Self::Run),
            "time" => Some(Self::Time),
            _ => None,
        }
    }
}

/// A watch as kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub id: Uuid,
    pub boss: String,
    pub person: String,
    pub kind: WatchKind,
    pub repo: Option<String>,
    pub number: Option<i64>,
    pub run_id: Option<i64>,
    pub until_merged: bool,
    /// The boss's own words: why it waits and what it does next.
    pub note: String,
    pub session: Option<Uuid>,
    pub plan: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub due_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub polled_at: Option<DateTime<Utc>>,
    /// Reads in a row that failed.
    pub failures: i32,
    pub ended_at: Option<DateTime<Utc>>,
    /// How it ended, in words for the boss.
    pub outcome: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
}

impl Watch {
    /// What it waits on, in a few words.
    pub fn what(&self) -> String {
        let repo = self.repo.as_deref().unwrap_or("?");
        match self.kind {
            WatchKind::Pr => {
                let until = match self.until_merged {
                    true => "until it merges or closes",
                    false => "until its checks are done",
                };
                format!("PR #{} in {repo}, {until}", self.number.unwrap_or_default())
            }
            WatchKind::Run => format!("CI run {} in {repo}", self.run_id.unwrap_or_default()),
            WatchKind::Time => match self.due_at {
                Some(at) => format!("a time, {}", at.format("%Y-%m-%d %H:%M UTC")),
                None => "a time".into(),
            },
        }
    }
}

/// A watch to start.
#[derive(Debug, Clone)]
pub struct NewWatch {
    pub boss: String,
    pub person: String,
    pub kind: WatchKind,
    pub repo: Option<String>,
    pub number: Option<i64>,
    pub run_id: Option<i64>,
    pub until_merged: bool,
    pub note: String,
    pub session: Option<Uuid>,
    pub plan: Option<Uuid>,
    pub due_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
}

/// What became of a watch asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    Started(Watch),
    /// The same wait is open already: that one.
    Already(Watch),
    /// [`MAX_OPEN`] are open: none started.
    Full(Vec<Watch>),
}

const COLUMNS: &str = "id, boss, person, kind, repo, number, run_id, until_merged, note, \
    session, plan, created_at, due_at, expires_at, polled_at, failures, ended_at, outcome, \
    delivered_at";

fn watch_of(r: &Row) -> Result<Watch> {
    let kind: String = r.try_get(3)?;
    Ok(Watch {
        id: r.try_get(0)?,
        boss: r.try_get(1)?,
        person: r.try_get(2)?,
        kind: WatchKind::parse(&kind).ok_or_else(|| eyre::eyre!("a watch of kind {kind}"))?,
        repo: r.try_get(4)?,
        number: r.try_get(5)?,
        run_id: r.try_get(6)?,
        until_merged: r.try_get(7)?,
        note: r.try_get(8)?,
        session: r.try_get(9)?,
        plan: r.try_get(10)?,
        created_at: r.try_get(11)?,
        due_at: r.try_get(12)?,
        expires_at: r.try_get(13)?,
        polled_at: r.try_get(14)?,
        failures: r.try_get(15)?,
        ended_at: r.try_get(16)?,
        outcome: r.try_get(17)?,
        delivered_at: r.try_get(18)?,
    })
}

impl DeskStore {
    /// Starts `w`, unless the same wait is open already for its boss and
    /// person, or [`MAX_OPEN`] are: checked and kept in one transaction.
    pub async fn watch_open(&self, w: &NewWatch) -> Result<Opened> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // One boss and person's watches at a time.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('boss_watches:' || $1 || ':' || $2))",
            &[&w.boss, &w.person],
        )
        .await?;
        let open: Vec<Watch> = tx
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_watches \
                     WHERE boss = $1 AND person = $2 AND ended_at IS NULL ORDER BY created_at"
                ),
                &[&w.boss, &w.person],
            )
            .await?
            .iter()
            .map(watch_of)
            .collect::<Result<_>>()?;
        let same = open.iter().find(|o| {
            o.kind == w.kind
                && o.repo == w.repo
                && o.number == w.number
                && o.run_id == w.run_id
                && o.until_merged == w.until_merged
                && w.kind != WatchKind::Time
        });
        if let Some(same) = same {
            return Ok(Opened::Already(same.clone()));
        }
        if open.len() >= MAX_OPEN {
            return Ok(Opened::Full(open));
        }
        let id = Uuid::new_v4();
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO boss_watches (id, boss, person, kind, repo, number, run_id, \
                       until_merged, note, session, plan, created_at, due_at, expires_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), $12, $13) \
                     RETURNING {COLUMNS}"
                ),
                &[
                    &id,
                    &w.boss,
                    &w.person,
                    &w.kind.as_str(),
                    &w.repo,
                    &w.number,
                    &w.run_id,
                    &w.until_merged,
                    &w.note,
                    &w.session,
                    &w.plan,
                    &w.due_at,
                    &w.expires_at,
                ],
            )
            .await
            .context("starting a watch")?;
        let made = watch_of(&row)?;
        tx.commit().await?;
        Ok(Opened::Started(made))
    }

    /// Every open watch, oldest first: what the hub polls.
    pub async fn watches_open_all(&self) -> Result<Vec<Watch>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_watches WHERE ended_at IS NULL ORDER BY created_at"
                ),
                &[],
            )
            .await?
            .iter()
            .map(watch_of)
            .collect()
    }

    /// `boss`'s open watches for `person`, oldest first.
    pub async fn watches_open(&self, boss: &str, person: &str) -> Result<Vec<Watch>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_watches \
                     WHERE boss = $1 AND person = $2 AND ended_at IS NULL ORDER BY created_at"
                ),
                &[&boss, &person],
            )
            .await?
            .iter()
            .map(watch_of)
            .collect()
    }

    /// The watches that ended and whose end the boss hasn't been told yet.
    pub async fn watches_undelivered(&self) -> Result<Vec<Watch>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_watches \
                     WHERE ended_at IS NOT NULL AND delivered_at IS NULL ORDER BY ended_at"
                ),
                &[],
            )
            .await?
            .iter()
            .map(watch_of)
            .collect()
    }

    /// Notes that watch `id` was read: a failed read counts towards its
    /// backing off, a good one clears that.
    pub async fn watch_polled(&self, id: Uuid, failed: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_watches SET polled_at = now(), \
                   failures = CASE WHEN $2 THEN failures + 1 ELSE 0 END \
                 WHERE id = $1 AND ended_at IS NULL",
                &[&id, &failed],
            )
            .await?;
        Ok(())
    }

    /// Ends watch `id` with `outcome`, once: the watch when this ended it.
    pub async fn watch_end(&self, id: Uuid, outcome: &str) -> Result<Option<Watch>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                &format!(
                    "UPDATE boss_watches SET ended_at = now(), outcome = $2 \
                     WHERE id = $1 AND ended_at IS NULL RETURNING {COLUMNS}"
                ),
                &[&id, &outcome],
            )
            .await?;
        row.as_ref().map(watch_of).transpose()
    }

    /// Ends `boss`'s open watch `id` for `person` as the boss asked, told
    /// as delivered (it knows): whether there was one.
    pub async fn watch_cancel(&self, boss: &str, person: &str, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE boss_watches SET ended_at = now(), outcome = 'ended by the boss', \
                   delivered_at = now() \
                 WHERE id = $1 AND boss = $2 AND person = $3 AND ended_at IS NULL",
                &[&id, &boss, &person],
            )
            .await?;
        Ok(n > 0)
    }

    /// Notes that the boss was told the ends of `ids`.
    pub async fn watches_delivered(&self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_watches SET delivered_at = now() \
                 WHERE id = ANY($1) AND ended_at IS NOT NULL AND delivered_at IS NULL",
                &[&ids],
            )
            .await?;
        Ok(())
    }
}
