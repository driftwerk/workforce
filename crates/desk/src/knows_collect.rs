//! What the sync reads of each kind it shares (Sees K3,
//! `designs/agent-sees.md` §3 in workforce-desk), in Postgres.
//!
//! - **Accounts, needs and decisions** are read whole (within the backfill
//!   for needs and decisions): the sync sends each again when its body
//!   changes, so a renamed session, a closed need or an undone decision
//!   reaches the agent.
//! - **Board activity and transcripts** are sent once: what the agent wasn't
//!   sent yet (or was, and it was deleted), within the backfill, of what its
//!   rule covers, oldest first. With no cursor, a widened rule's sessions
//!   and a late harvest are found as surely as a new row.
//!
//! A session whose memory is off is never read, nor what of a group whose
//! harvest is off.

use chrono::{DateTime, Utc};
use eyre::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::knows::TRANSCRIPT;
use crate::store::DeskStore;

/// A session's account, with what the session is.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub session: Uuid,
    pub name: String,
    pub engine: String,
    pub cwd: String,
    pub state: String,
    pub purpose: String,
    pub plan: String,
    pub now: String,
    pub next: String,
    pub waiting_on: Option<String>,
    pub done: Value,
    pub decisions: Value,
    pub written_at: DateTime<Utc>,
}

/// A boss's question or confirmation waiting on the person.
#[derive(Debug, Clone, PartialEq)]
pub struct NeedRow {
    pub id: Uuid,
    pub boss: String,
    pub kind: String,
    pub session: Option<Uuid>,
    /// Every session it is about, `session` among them.
    pub sessions: Vec<Uuid>,
    pub session_name: Option<String>,
    pub text: String,
    pub holding_up: Option<String>,
    pub state: String,
    pub opened_at: DateTime<Utc>,
    pub changed_at: DateTime<Utc>,
}

/// A boss's decision.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRow {
    pub id: Uuid,
    pub boss: String,
    pub session: Option<Uuid>,
    pub session_name: Option<String>,
    pub need: String,
    pub what: String,
    pub why: String,
    pub how: String,
    pub at: DateTime<Utc>,
    pub undone_at: Option<DateTime<Utc>>,
}

/// Something that happened on a board's ticket.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardRow {
    pub id: String,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub by: String,
    pub text: String,
    pub board: Uuid,
    pub board_name: String,
    pub group: Option<String>,
    pub ticket: String,
    pub title: String,
}

/// A closed exchange of a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeRow {
    /// `<engine>:<key>`, as the agent's copy is keyed.
    pub reference: String,
    pub session: Uuid,
    pub session_name: Option<String>,
    pub group: Option<String>,
    pub at: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub asker: String,
    pub ask: String,
    pub answer: String,
    pub steps: Value,
    pub files: Vec<String>,
    pub commands: Vec<String>,
    pub errors: Vec<String>,
    pub flagged: Vec<String>,
}

/// What of the append kinds a rule covers, for the reads below: everything,
/// or these sessions, boards and groups. Groups whose harvest is off are
/// left out either way.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub everything: bool,
    pub sessions: Vec<Uuid>,
    pub boards: Vec<Uuid>,
    pub groups: Vec<String>,
    pub groups_off: Vec<String>,
    /// Sessions in a group whose harvest is off now, wherever they were
    /// harvested.
    pub sessions_off: Vec<Uuid>,
}

/// The ref an exchange is shared by.
pub fn exchange_ref(engine: &str, key: &str) -> String {
    format!("{engine}:{key}")
}

/// Not yet sent to `$1` as `kind` by `ref_sql`, or sent and deleted: the
/// append kinds' anti-join. Being deleted counts as sent.
fn unsent(kind: &str, ref_sql: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM knows_sent k WHERE k.agent = $1 AND k.kind = '{kind}' \
         AND k.ref = {ref_sql} AND (k.deleted_at IS NULL OR k.deleting IS NOT NULL))"
    )
}

impl DeskStore {
    /// The groups whose `memory.group.<id>` says not to harvest.
    pub async fn knows_groups_off(&self) -> Result<Vec<String>> {
        let client = self.pool().get().await?;
        Ok(client
            .query(
                "SELECT key FROM settings WHERE key LIKE 'memory.group.%' \
                 AND value->>'harvest' = 'false'",
                &[],
            )
            .await?
            .iter()
            .filter_map(|r| {
                let k: String = r.get(0);
                k.strip_prefix("memory.group.").map(str::to_string)
            })
            .collect())
    }

    /// Every account of a session whose memory is on.
    pub async fn knows_accounts(&self) -> Result<Vec<AccountRow>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT a.session, s.name, s.engine, s.cwd, s.state, a.purpose, a.plan, a.now, \
                   a.next, a.waiting_on, a.done, a.decisions, a.written_at \
                 FROM session_accounts a JOIN sessions s ON s.id = a.session \
                 WHERE s.memory = 'on' \
                   AND NOT EXISTS (SELECT 1 FROM knows_forgotten f WHERE f.session = a.session) \
                 ORDER BY a.written_at",
                &[],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(AccountRow {
                    session: r.try_get(0)?,
                    name: r.try_get(1)?,
                    engine: r.try_get(2)?,
                    cwd: r.try_get(3)?,
                    state: r.try_get(4)?,
                    purpose: r.try_get(5)?,
                    plan: r.try_get(6)?,
                    now: r.try_get(7)?,
                    next: r.try_get(8)?,
                    waiting_on: r.try_get(9)?,
                    done: r.try_get(10)?,
                    decisions: r.try_get(11)?,
                    written_at: r.try_get(12)?,
                })
            })
            .collect()
    }

    /// The bosses' questions and confirmations for the person: open ones,
    /// and those changed since `since`. Of a session whose memory is off,
    /// none.
    pub async fn knows_needs(&self, since: DateTime<Utc>) -> Result<Vec<NeedRow>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT o.id, o.boss, o.kind, o.session, s.name, o.text, o.holding_up, o.state, \
                   o.opened_at, o.changed_at, o.sessions \
                 FROM boss_open_items o LEFT JOIN sessions s ON s.id = o.session \
                 WHERE o.kind IN ('ask', 'confirm') AND (o.state <> 'closed' OR o.changed_at >= $1) \
                   AND (o.session IS NULL OR s.memory = 'on') \
                   AND NOT EXISTS (SELECT 1 FROM unnest(o.sessions) x(id) \
                     LEFT JOIN sessions y ON y.id = x.id \
                     WHERE y.id IS NULL OR y.memory <> 'on') \
                   AND NOT EXISTS (SELECT 1 FROM knows_forgotten f \
                     WHERE f.session = o.session OR f.session = ANY(o.sessions)) \
                 ORDER BY o.changed_at",
                &[&since],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(NeedRow {
                    id: r.try_get(0)?,
                    boss: r.try_get(1)?,
                    kind: r.try_get(2)?,
                    session: r.try_get(3)?,
                    session_name: r.try_get(4)?,
                    text: r.try_get(5)?,
                    holding_up: r.try_get(6)?,
                    state: r.try_get(7)?,
                    opened_at: r.try_get(8)?,
                    changed_at: r.try_get(9)?,
                    sessions: r.try_get(10)?,
                })
            })
            .collect()
    }

    /// The bosses' decisions made or undone since `since`. Of a session
    /// whose memory is off, none.
    pub async fn knows_decisions(&self, since: DateTime<Utc>) -> Result<Vec<DecisionRow>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT d.id, d.boss, d.session, s.name, d.need, d.what, d.why, d.how, d.at, \
                   d.undone_at \
                 FROM boss_decisions d LEFT JOIN sessions s ON s.id = d.session \
                 WHERE coalesce(d.undone_at, d.at) >= $1 \
                   AND (d.session IS NULL OR s.memory = 'on') \
                   AND NOT EXISTS (SELECT 1 FROM knows_forgotten f WHERE f.session = d.session) \
                 ORDER BY d.at",
                &[&since],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(DecisionRow {
                    id: r.try_get(0)?,
                    boss: r.try_get(1)?,
                    session: r.try_get(2)?,
                    session_name: r.try_get(3)?,
                    need: r.try_get(4)?,
                    what: r.try_get(5)?,
                    why: r.try_get(6)?,
                    how: r.try_get(7)?,
                    at: r.try_get(8)?,
                    undone_at: r.try_get(9)?,
                })
            })
            .collect()
    }

    /// What happened on the tickets of the boards `scope` covers since
    /// `since` that `agent` wasn't sent, oldest first, at most `limit`.
    pub async fn knows_board_activity(
        &self,
        agent: &str,
        scope: &Scope,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<BoardRow>> {
        let client = self.pool().get().await?;
        let sql = format!(
            "SELECT a.id, a.at, a.kind, coalesce(a.actor->>'name', a.actor->>'kind', ''), a.text, \
               b.id, b.name, b.group_id, coalesce(b.prefix, 'T') || '-' || t.number, t.title \
             FROM activity a JOIN tickets t ON t.id = a.ticket JOIN boards b ON b.id = t.board \
             WHERE a.at >= $2 \
               AND ($3 OR b.id = ANY($4) OR b.group_id = ANY($5)) \
               AND (b.group_id IS NULL OR NOT b.group_id = ANY($6)) \
               AND {} \
             ORDER BY a.at, a.id LIMIT $7",
            unsent("board", "a.id")
        );
        client
            .query(
                &sql,
                &[
                    &agent,
                    &since,
                    &scope.everything,
                    &scope.boards,
                    &scope.groups,
                    &scope.groups_off,
                    &limit,
                ],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(BoardRow {
                    id: r.try_get(0)?,
                    at: r.try_get(1)?,
                    kind: r.try_get(2)?,
                    by: r.try_get(3)?,
                    text: r.try_get(4)?,
                    board: r.try_get(5)?,
                    board_name: r.try_get(6)?,
                    group: r.try_get(7)?,
                    ticket: r.try_get(8)?,
                    title: r.try_get(9)?,
                })
            })
            .collect()
    }

    /// The closed exchanges of the sessions `scope` covers, ended since
    /// `since`, that `agent` wasn't sent, oldest first, at most `limit`.
    pub async fn knows_exchanges(
        &self,
        agent: &str,
        scope: &Scope,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ExchangeRow>> {
        let client = self.pool().get().await?;
        let sql = format!(
            "SELECT e.engine || ':' || e.key, e.session, s.name, e.group_id, e.at, e.ended, \
               e.asker, e.ask, e.answer, e.steps, e.files, e.commands, e.errors, e.flagged \
             FROM memory_exchanges e JOIN sessions s ON s.id = e.session \
             WHERE e.ended IS NOT NULL AND e.ended >= $2 \
               AND ($3 OR e.session = ANY($4)) \
               AND (e.group_id IS NULL OR NOT e.group_id = ANY($5)) \
               AND NOT e.session = ANY($7) \
               AND s.memory = 'on' \
               AND NOT EXISTS (SELECT 1 FROM knows_forgotten f WHERE f.session = e.session) \
               AND {} \
             ORDER BY e.ended, e.id LIMIT $6",
            unsent(TRANSCRIPT, "e.engine || ':' || e.key")
        );
        client
            .query(
                &sql,
                &[
                    &agent,
                    &since,
                    &scope.everything,
                    &scope.sessions,
                    &scope.groups_off,
                    &limit,
                    &scope.sessions_off,
                ],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(ExchangeRow {
                    reference: r.try_get(0)?,
                    session: r.try_get(1)?,
                    session_name: r.try_get(2)?,
                    group: r.try_get(3)?,
                    at: r.try_get(4)?,
                    ended: r.try_get(5)?,
                    asker: r.try_get(6)?,
                    ask: r.try_get(7)?,
                    answer: r.try_get(8)?,
                    steps: r.try_get(9)?,
                    files: r.try_get(10)?,
                    commands: r.try_get(11)?,
                    errors: r.try_get(12)?,
                    flagged: r.try_get(13)?,
                })
            })
            .collect()
    }
}
