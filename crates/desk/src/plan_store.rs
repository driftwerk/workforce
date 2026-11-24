//! Plans in Postgres: `plans`, written whole on every change; each plan's
//! `plan_revisions`, added and never changed; the plan columns of its
//! tickets; and the `starting` links that make a start happen once.
//!
//! A start is two steps across a process boundary: the ticket moves to
//! Doing with a `starting` link naming the session about to start, in one
//! transaction, and only then is the session started with that id. A
//! `starting` link left by a runner stopped between the two names a
//! session that either runs (the start finishes) or doesn't (it is started
//! again with the same id), so a ticket is never started twice.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::board::{Ticket, TicketState};
use crate::engine::{Engine, View};
use crate::goal::{Met, MetBy};
use crate::plan::{Change, Paused, Plan, PlanState, Revision};
use crate::store::DeskStore;

pub(crate) const PLAN: &str =
    "id, board, group_id, boss, goal, state, paused, parallel, revision, \
                    approved_revision, created_at, updated_at, paused_why, result, agreed, \
                    agreed_at, goal_ticket, read_back_at, met_at, met_by, met_why";
const REVISION: &str = "plan, revision, at, made_by, change, summary, snapshot";

/// The `ticket_links` kind of a session about to start for a ticket.
pub const STARTING: &str = "starting";

pub(crate) fn plan(row: &Row) -> Result<Plan> {
    let paused: Option<String> = row.try_get("paused")?;
    let result: Option<serde_json::Value> = row.try_get("result")?;
    let met_at: Option<DateTime<Utc>> = row.try_get("met_at")?;
    let met_by: Option<String> = row.try_get("met_by")?;
    let met = match (met_at, met_by) {
        (Some(at), Some(by)) => Some(Met {
            at,
            by: MetBy::parse(&by)?,
            why: row
                .try_get::<_, Option<String>>("met_why")?
                .unwrap_or_default(),
        }),
        _ => None,
    };
    Ok(Plan {
        id: row.try_get("id")?,
        board: row.try_get("board")?,
        group: row.try_get("group_id")?,
        boss: row.try_get("boss")?,
        goal: row.try_get("goal")?,
        state: PlanState::parse(row.try_get("state")?)?,
        paused: paused.as_deref().map(Paused::parse).transpose()?,
        paused_why: row.try_get("paused_why")?,
        parallel: row.try_get("parallel")?,
        revision: row.try_get("revision")?,
        approved_revision: row.try_get("approved_revision")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        result: result
            .map(serde_json::from_value)
            .transpose()
            .context("reading a plan's result")?,
        agreed: row.try_get("agreed")?,
        agreed_at: row.try_get("agreed_at")?,
        goal_ticket: row.try_get("goal_ticket")?,
        read_back_at: row.try_get("read_back_at")?,
        met,
    })
}

fn revision(row: &Row) -> Result<Revision> {
    Ok(Revision {
        plan: row.try_get("plan")?,
        revision: row.try_get("revision")?,
        at: row.try_get("at")?,
        by: serde_json::from_value(row.try_get("made_by")?)
            .context("reading who made a revision")?,
        change: Change::parse(row.try_get("change")?)?,
        summary: row.try_get("summary")?,
        snapshot: serde_json::from_value(row.try_get("snapshot")?)
            .context("reading a plan's revision")?,
    })
}

/// Why a start wasn't begun.
#[derive(Debug, thiserror::Error)]
pub enum BeginStart {
    /// The ticket changed since it was read.
    #[error("the ticket changed since it was read")]
    Stale,
    /// Its plan doesn't run now: paused, dropped or done meanwhile.
    #[error("the plan isn't running")]
    NotRunning,
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

impl From<tokio_postgres::Error> for BeginStart {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Other(eyre::Report::new(e).wrap_err("beginning a start"))
    }
}

impl From<workforce_postgres::PostgresError> for BeginStart {
    fn from(e: workforce_postgres::PostgresError) -> Self {
        Self::Other(eyre::Report::new(e))
    }
}

/// Writes `p` whole, but when and how its goal was met and said back:
/// [`DeskStore::mark_met`] and [`DeskStore::set_read_back`] write those,
/// once, so a plan written from an older read never takes them back.
async fn put_plan_in<C: tokio_postgres::GenericClient>(client: &C, p: &Plan) -> Result<()> {
    let result = p.result.as_ref().map(serde_json::to_value).transpose()?;
    client
        .execute(
            "INSERT INTO plans (id, board, group_id, boss, goal, state, paused, parallel, \
             revision, approved_revision, created_at, updated_at, paused_why, result, agreed, \
             agreed_at, goal_ticket) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
             ON CONFLICT (id) DO UPDATE SET board = EXCLUDED.board, group_id = EXCLUDED.group_id, \
             boss = EXCLUDED.boss, goal = EXCLUDED.goal, state = EXCLUDED.state, \
             paused = EXCLUDED.paused, paused_why = EXCLUDED.paused_why, \
             parallel = EXCLUDED.parallel, \
             revision = EXCLUDED.revision, approved_revision = EXCLUDED.approved_revision, \
             updated_at = EXCLUDED.updated_at, result = EXCLUDED.result, \
             agreed = EXCLUDED.agreed, agreed_at = EXCLUDED.agreed_at, \
             goal_ticket = EXCLUDED.goal_ticket",
            &[
                &p.id,
                &p.board,
                &p.group,
                &p.boss,
                &p.goal,
                &p.state.as_str(),
                &p.paused.map(Paused::as_str),
                &p.parallel,
                &p.revision,
                &p.approved_revision,
                &p.created_at,
                &p.updated_at,
                &p.paused_why,
                &result,
                &p.agreed,
                &p.agreed_at,
                &p.goal_ticket,
            ],
        )
        .await?;
    Ok(())
}

/// Where a plan's ticket runs, as its row says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Runs {
    pub plan: Option<Uuid>,
    pub engine: Option<Engine>,
    pub folder: Option<String>,
    pub view: Option<View>,
    /// The person's machine it runs on; none is this computer.
    pub machine: Option<String>,
    /// The model its session runs; none is its engine's default.
    pub model: Option<String>,
    /// The repository it works in (`github.com/owner/name`): its folder on
    /// the computer it runs on is that repository's checkout there.
    pub repo: Option<String>,
    /// Taken on from a board, not made by the plan: never deleted, dropped,
    /// renamed or re-filed by it. Read only: [`DeskStore::put_runs`] leaves
    /// it, [`DeskStore::adopt_ticket`] and [`DeskStore::release_ticket`]
    /// set it.
    pub adopted: bool,
    /// Why it waits in To do for someone to start it, its plan not
    /// starting it by itself. Read only: [`DeskStore::put_ticket_holding`]
    /// sets it, and starting it, handing it out, moving it to another
    /// column or letting it go from its plan clears it.
    pub held_back: Option<String>,
    /// The session it is assigned to was closed or removed, so it holds no
    /// slot of its plan. Read only: [`DeskStore::close_session_tickets`]
    /// sets it, and the ticket going to anyone else clears it.
    pub session_closed: bool,
}

/// What a ticket a plan takes on must be as it is claimed: no one has it,
/// it hasn't started, no open plan has it, and no session is starting for
/// it. `$1` is the ticket.
const ADOPTABLE: &str = "assignee IS NULL AND state IN ('backlog', 'todo') \
     AND (plan IS NULL OR plan IN (SELECT id FROM plans WHERE state IN ('done', 'dropped'))) \
     AND NOT EXISTS (SELECT 1 FROM ticket_links l WHERE l.ticket = $1 AND l.kind = 'starting')";

/// Why a ticket couldn't be taken on.
#[derive(Debug, thiserror::Error)]
pub enum Adopt {
    /// Someone has it, it started, another plan has it, or it is gone:
    /// read it again to say which.
    #[error("the ticket can't be taken on as it is")]
    Held(Uuid),
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

impl From<tokio_postgres::Error> for Adopt {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Other(eyre::Report::new(e).wrap_err("taking a ticket on"))
    }
}

impl From<serde_json::Error> for Adopt {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(eyre::Report::new(e))
    }
}

impl From<workforce_postgres::PostgresError> for Adopt {
    fn from(e: workforce_postgres::PostgresError) -> Self {
        Self::Other(eyre::Report::new(e))
    }
}

/// A ticket a plan takes on, as it is claimed: where it runs there, the
/// needs it is given, and the running session it stands for, when it is
/// that session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub ticket: Uuid,
    pub runs: Runs,
    pub needs: Vec<Uuid>,
    pub session: Option<Uuid>,
}

/// Claims `ticket` for plan `runs.plan`, while it is [`ADOPTABLE`]; or,
/// for the running `session` it stands for, while that session has it and
/// no other open plan does.
async fn adopt_in<C: tokio_postgres::GenericClient>(
    client: &C,
    ticket: Uuid,
    runs: &Runs,
    session: Option<Uuid>,
) -> Result<(), Adopt> {
    let n = client
        .execute(
            &format!(
                "UPDATE tickets SET plan = $2, engine = $3, folder = $4, view = $5, machine = $6, \
                 model = $7, repo = $8, adopted = true, version = version + 1 \
                 WHERE id = $1 AND CASE WHEN $9::text IS NULL \
                 THEN ({ADOPTABLE}) OR (plan = $2 AND adopted AND assignee IS NULL \
                   AND state IN ('backlog', 'todo')) \
                 ELSE assignee ->> 'kind' = 'session' AND assignee ->> 'id' = $9 \
                   AND (plan IS NULL OR plan = $2 \
                     OR plan IN (SELECT id FROM plans WHERE state IN ('done', 'dropped'))) END"
            ),
            &[
                &ticket,
                &runs.plan,
                &runs.engine.map(Engine::as_str),
                &runs.folder,
                &runs.view.map(View::as_str),
                &runs.machine,
                &runs.model,
                &runs.repo,
                &session.map(|s| s.to_string()),
            ],
        )
        .await?;
    match n {
        1 => Ok(()),
        _ => Err(Adopt::Held(ticket)),
    }
}

/// A `starting` link: the session about to start for a ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starting {
    pub ticket: Uuid,
    pub session: Uuid,
    pub at: DateTime<Utc>,
}

impl DeskStore {
    /// Writes `p` whole, new or changed.
    pub async fn put_plan(&self, p: &Plan) -> Result<()> {
        let client = self.pool().get().await?;
        put_plan_in(&**client, p).await
    }

    /// Re-files plan `p` on board `to`, in one step: `p` written as it
    /// says (its board `to`), and each of its own tickets (never one it took
    /// on, which stays on its board) numbered on `to` in
    /// the order they were numbered, still in the plan, with what they
    /// need, their links, history and assignment kept. The tickets as they
    /// are now, in that order.
    pub async fn move_plan_board(&self, p: &Plan, to: Uuid) -> Result<Vec<Ticket>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let ids: Vec<Uuid> = tx
            .query(
                "SELECT id FROM tickets WHERE plan = $1 AND board <> $2 AND NOT adopted \
                 ORDER BY number",
                &[&p.id, &to],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<Result<_, _>>()?;
        for id in &ids {
            let number: i32 = tx
                .query_one(
                    "UPDATE boards SET next_number = next_number + 1 WHERE id = $1 \
                     RETURNING next_number - 1",
                    &[&to],
                )
                .await
                .context("numbering a re-filed plan's ticket")?
                .try_get(0)?;
            tx.execute(
                "UPDATE tickets SET board = $2, number = $3, updated_at = now(), \
                 version = version + 1 WHERE id = $1",
                &[id, &to, &number],
            )
            .await?;
        }
        put_plan_in(&*tx, p).await?;
        tx.commit()
            .await
            .context("re-filing a plan on another board")?;
        drop(client);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = self.ticket(id).await? {
                out.push(t);
            }
        }
        Ok(out)
    }

    pub async fn plan(&self, id: Uuid) -> Result<Option<Plan>> {
        let client = self.pool().get().await?;
        client
            .query_opt(&format!("SELECT {PLAN} FROM plans WHERE id = $1"), &[&id])
            .await?
            .as_ref()
            .map(plan)
            .transpose()
    }

    /// Every plan, newest first; only those in `state` when it is said.
    pub async fn plans(&self, state: Option<PlanState>) -> Result<Vec<Plan>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {PLAN} FROM plans WHERE $1::text IS NULL OR state = $1 \
                     ORDER BY created_at DESC, id"
                ),
                &[&state.map(PlanState::as_str)],
            )
            .await?
            .iter()
            .map(plan)
            .collect()
    }

    /// Adds `r`; a revision is never written twice, so two edits from the
    /// same read can't both be kept.
    pub async fn add_revision(&self, r: &Revision) -> Result<bool> {
        let client = self.pool().get().await?;
        let added = client
            .execute(
                &format!(
                    "INSERT INTO plan_revisions ({REVISION}) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                     ON CONFLICT (plan, revision) DO NOTHING"
                ),
                &[
                    &r.plan,
                    &r.revision,
                    &r.at,
                    &serde_json::to_value(&r.by)?,
                    &r.change.as_str(),
                    &r.summary,
                    &serde_json::to_value(&r.snapshot)?,
                ],
            )
            .await?;
        Ok(added == 1)
    }

    /// A plan's revisions, oldest first.
    pub async fn revisions(&self, plan_id: Uuid) -> Result<Vec<Revision>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {REVISION} FROM plan_revisions WHERE plan = $1 ORDER BY revision"),
                &[&plan_id],
            )
            .await?
            .iter()
            .map(revision)
            .collect()
    }

    pub async fn revision(&self, plan_id: Uuid, n: i32) -> Result<Option<Revision>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {REVISION} FROM plan_revisions WHERE plan = $1 AND revision = $2"),
                &[&plan_id, &n],
            )
            .await?
            .as_ref()
            .map(revision)
            .transpose()
    }

    /// Writes where `ticket` runs, and which plan it is in. The ticket's
    /// version moves on: a start read before is refused. One taken on that
    /// is no longer the plan's (let go, or another plan's now) is left as
    /// it is: only a claim takes it again. A folder left empty for its
    /// repository to say keeps the checkout a start found, while the
    /// repository and machine are the same, when `keep_found`.
    pub async fn put_runs(&self, ticket: Uuid, runs: &Runs, keep_found: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE tickets SET plan = $2, engine = $3, \
                 folder = CASE WHEN $4 = '' AND $9 AND repo IS NOT DISTINCT FROM $8 AND machine IS NOT DISTINCT FROM $6 \
                 THEN folder ELSE $4 END, view = $5, \
                 machine = $6, model = $7, repo = $8, version = version + 1 \
                 WHERE id = $1 AND (plan = $2 OR (plan IS NULL AND NOT adopted))",
                &[
                    &ticket,
                    &runs.plan,
                    &runs.engine.map(Engine::as_str),
                    &runs.folder,
                    &runs.view.map(View::as_str),
                    &runs.machine,
                    &runs.model,
                    &runs.repo,
                    &keep_found,
                ],
            )
            .await?;
        Ok(())
    }

    /// The checkout a start found for `ticket`'s repository saved as its
    /// folder, only while the ticket is as it was read (its `version`, its
    /// `repo`, `machine` and folder `was`): whether it was. Nothing the person changed
    /// meanwhile is written over.
    pub async fn found_folder(
        &self,
        ticket: Uuid,
        folder: &str,
        repo: &str,
        machine: Option<&str>,
        was: Option<&str>,
        version: i32,
    ) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE tickets SET folder = $2, version = version + 1 \
                 WHERE id = $1 AND repo = $3 AND machine IS NOT DISTINCT FROM $4 AND version = $5 \
                 AND folder IS NOT DISTINCT FROM $6",
                &[&ticket, &folder, &repo, &machine, &version, &was],
            )
            .await?;
        Ok(n == 1)
    }

    /// Where each of `plan_id`'s tickets runs, by ticket.
    pub async fn plan_runs(&self, plan_id: Uuid) -> Result<Vec<(Uuid, Runs)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, plan, engine, folder, view, machine, model, repo, adopted, held_back, \
                 session_closed_at IS NOT NULL AS session_closed FROM tickets WHERE plan = $1",
                &[&plan_id],
            )
            .await?
            .iter()
            .map(|r| {
                let engine: Option<String> = r.try_get("engine")?;
                let view: Option<String> = r.try_get("view")?;
                Ok((
                    r.try_get("id")?,
                    Runs {
                        plan: r.try_get("plan")?,
                        engine: engine.as_deref().map(Engine::parse).transpose()?,
                        folder: r.try_get("folder")?,
                        view: view.as_deref().map(View::parse).transpose()?,
                        machine: r.try_get("machine")?,
                        model: r.try_get("model")?,
                        repo: r.try_get("repo")?,
                        adopted: r.try_get("adopted")?,
                        held_back: r.try_get("held_back")?,
                        session_closed: r.try_get("session_closed")?,
                    },
                ))
            })
            .collect()
    }

    /// The plan `ticket` is in, if any.
    pub async fn plan_of_ticket(&self, ticket: Uuid) -> Result<Option<Uuid>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt("SELECT plan FROM tickets WHERE id = $1", &[&ticket])
            .await?
            .map(|r| r.try_get(0))
            .transpose()?
            .flatten())
    }

    /// Moves `t`, as it was read, to Doing at `rank` with a `starting` link
    /// to `session`, in one transaction, while its plan `plan` runs: what it
    /// runs in, as the row says then. Refused as stale when the ticket
    /// changed since it was read (the person moved it, or the boss changed
    /// where it runs), and when the plan was paused, dropped or done
    /// meanwhile: the plan's row is held until the link is written, so a
    /// pause either comes first and nothing starts, or waits for this one.
    pub async fn begin_start(
        &self,
        t: &mut Ticket,
        plan: Uuid,
        rank: f64,
        session: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Runs, BeginStart> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let runs = tx
            .query_opt(
                "SELECT 1 FROM plans WHERE id = $1 AND state = 'running' AND paused IS NULL \
                 AND approved_revision IS NOT NULL FOR SHARE",
                &[&plan],
            )
            .await?;
        if runs.is_none() {
            return Err(BeginStart::NotRunning);
        }
        let row = tx
            .query_opt(
                "UPDATE tickets SET state = $3, rank = $4, state_at = $5, updated_at = $5, \
                 held_back = NULL, version = version + 1 WHERE id = $1 AND version = $2 AND plan = $6 \
                 RETURNING version, engine, folder, view, machine, model, adopted, repo",
                &[
                    &t.id,
                    &t.version,
                    &TicketState::Doing.as_str(),
                    &rank,
                    &at,
                    &plan,
                ],
            )
            .await?;
        let Some(row) = row else {
            return Err(BeginStart::Stale);
        };
        tx.execute(
            "INSERT INTO ticket_links (ticket, kind, target, at) VALUES ($1, $2, $3, $4)",
            &[&t.id, &STARTING, &session.to_string(), &at],
        )
        .await?;
        tx.commit().await?;
        let engine: Option<String> = row.try_get(1)?;
        let view: Option<String> = row.try_get(3)?;
        t.version = row.try_get(0)?;
        t.state = TicketState::Doing;
        t.rank = rank;
        t.state_at = at;
        t.updated_at = at;
        Ok(Runs {
            plan: Some(plan),
            engine: engine.as_deref().map(Engine::parse).transpose()?,
            folder: row.try_get(2)?,
            view: view.as_deref().map(View::parse).transpose()?,
            machine: row.try_get(4)?,
            model: row.try_get(5)?,
            adopted: row.try_get(6)?,
            held_back: None,
            session_closed: false,
            repo: row.try_get(7)?,
        })
    }

    /// Takes `ticket` on for the plan `runs.plan`, as it runs there, while
    /// no one has it, it hasn't started and no open plan has it; or, for
    /// the running `session` it stands for, while that session has it and
    /// no other open plan does.
    pub async fn adopt_ticket(
        &self,
        ticket: Uuid,
        runs: &Runs,
        session: Option<Uuid>,
    ) -> Result<(), Adopt> {
        let client = self.pool().get().await?;
        adopt_in(&**client, ticket, runs, session).await
    }

    /// Adds that `ticket` needs `needs`, as plan `plan`'s own; whether it
    /// was added (false: it was there already, and stays whose it was).
    pub async fn put_plan_need(&self, ticket: Uuid, needs: Uuid, plan: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "INSERT INTO ticket_needs (ticket, needs, plan) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
                &[&ticket, &needs, &plan],
            )
            .await?
            > 0)
    }

    /// The needs plan `plan` added that `ticket` is in, either way, as
    /// `(ticket, needs)`.
    pub async fn plan_added_needs(&self, plan: Uuid, ticket: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT ticket, needs FROM ticket_needs WHERE plan = $1 \
                 AND (ticket = $2 OR needs = $2)",
                &[&plan, &ticket],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// Lets `ticket` go from plan `plan`, where it is: no longer the
    /// plan's, with nothing said of where it runs, and without the needs
    /// `unneeded` (`(ticket, needs)`), the plan's own between its tickets.
    /// False when it wasn't the plan's.
    pub async fn release_ticket(
        &self,
        plan: Uuid,
        ticket: Uuid,
        unneeded: &[(Uuid, Uuid)],
    ) -> Result<bool> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute(
                "UPDATE tickets SET plan = NULL, engine = NULL, folder = NULL, view = NULL, \
                 machine = NULL, model = NULL, repo = NULL, adopted = false, held_back = NULL, \
                 version = version + 1 \
                 WHERE id = $1 AND plan = $2",
                &[&ticket, &plan],
            )
            .await?;
        if n == 1 {
            for (t, needs) in unneeded {
                tx.execute(
                    "DELETE FROM ticket_needs WHERE ticket = $1 AND needs = $2",
                    &[t, needs],
                )
                .await?;
            }
        }
        tx.commit()
            .await
            .context("letting a ticket go from a plan")?;
        Ok(n == 1)
    }

    /// Writes a new plan, its tickets (numbered on its board, with where
    /// each runs and what each needs), the tickets it takes on (`adopt`:
    /// each claimed with where it runs, and given what it needs) and its
    /// first revision, at once: a plan is never half there.
    pub async fn add_plan(
        &self,
        p: &Plan,
        tickets: &mut [(Ticket, Runs)],
        adopt: &[Claim],
        r: &Revision,
    ) -> Result<(), Adopt> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        put_plan_in(&*tx, p).await?;
        // Each ticket it takes on, claimed as it is now: one someone took
        // meanwhile refuses the whole plan.
        for c in adopt {
            adopt_in(&*tx, c.ticket, &c.runs, c.session).await?;
        }
        for (t, runs) in tickets.iter_mut() {
            let row = tx
                .query_one(
                    "UPDATE boards SET next_number = next_number + 1 WHERE id = $1 \
                     RETURNING next_number - 1",
                    &[&t.board],
                )
                .await
                .context("numbering a plan's ticket")?;
            t.number = row.try_get(0)?;
            crate::board_store::insert_ticket_in(&*tx, t).await?;
            tx.execute(
                "UPDATE tickets SET plan = $2, engine = $3, folder = $4, view = $5, \
                 machine = $6, model = $7, repo = $8 WHERE id = $1",
                &[
                    &t.id,
                    &runs.plan,
                    &runs.engine.map(Engine::as_str),
                    &runs.folder,
                    &runs.view.map(View::as_str),
                    &runs.machine,
                    &runs.model,
                    &runs.repo,
                ],
            )
            .await?;
        }
        let needs = tickets
            .iter()
            .map(|(t, _)| (t.id, t.needs.as_slice()))
            .chain(adopt.iter().map(|c| (c.ticket, c.needs.as_slice())));
        for (id, list) in needs {
            for n in list {
                // One there already (the person's, the tracker's) stays theirs.
                tx.execute(
                    "INSERT INTO ticket_needs (ticket, needs, plan) VALUES ($1, $2, $3) \
                     ON CONFLICT DO NOTHING",
                    &[&id, n, &p.id],
                )
                .await?;
            }
        }
        tx.execute(
            &format!("INSERT INTO plan_revisions ({REVISION}) VALUES ($1, $2, $3, $4, $5, $6, $7)"),
            &[
                &r.plan,
                &r.revision,
                &r.at,
                &serde_json::to_value(&r.by)?,
                &r.change.as_str(),
                &r.summary,
                &serde_json::to_value(&r.snapshot)?,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every `starting` link, oldest first.
    pub async fn starting(&self) -> Result<Vec<Starting>> {
        let client = self.pool().get().await?;
        let mut out = Vec::new();
        for r in client
            .query(
                "SELECT ticket, target, at FROM ticket_links WHERE kind = $1 ORDER BY at",
                &[&STARTING],
            )
            .await?
        {
            let target: String = r.try_get(1)?;
            // A target that isn't a session's id was never written here.
            let Ok(session) = target.parse() else {
                continue;
            };
            out.push(Starting {
                ticket: r.try_get(0)?,
                session,
                at: r.try_get(2)?,
            });
        }
        Ok(out)
    }

    /// The `starting` link of `ticket`, if it has one.
    pub async fn starting_of(&self, ticket: Uuid) -> Result<Option<Starting>> {
        Ok(self
            .starting()
            .await?
            .into_iter()
            .find(|s| s.ticket == ticket))
    }

    /// Removes `ticket`'s `starting` link to `session`: its start finished.
    pub async fn end_start(&self, ticket: Uuid, session: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM ticket_links WHERE ticket = $1 AND kind = $2 AND target = $3",
                &[&ticket, &STARTING, &session.to_string()],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::board::{Board, RANK_STEP};
    use crate::crew::Party;
    use crate::plan::{Planned, Snapshot};

    pub(crate) async fn board(s: &DeskStore) -> Board {
        let now = Utc::now();
        let b = Board {
            id: Uuid::new_v4(),
            name: "Acme".into(),
            group: Some("g-acme".into()),
            prefix: Some("ACM".into()),
            sync: None,
            next_number: 1,
            created_at: now,
            updated_at: now,
        };
        s.put_board(&b).await.unwrap();
        b
    }

    pub(crate) async fn ticket(s: &DeskStore, b: &Board, title: &str) -> Ticket {
        let now = Utc::now();
        let mut t = Ticket {
            id: Uuid::new_v4(),
            board: b.id,
            number: 0,
            title: title.into(),
            body: String::new(),
            state: TicketState::Todo,
            rank: RANK_STEP,
            assignee: None,
            needs: vec![],
            links: vec![],
            external: None,
            created_by: Party::manager("sarah", "Sarah"),
            pinned: false,
            snoozed_until: None,
            upstream_rank: None,
            snoozed_at: None,
            state_at: now,
            created_at: now,
            updated_at: now,
            version: 0,
        };
        s.add_ticket(&mut t).await.unwrap();
        t
    }

    pub(crate) fn new_plan(b: &Board) -> Plan {
        let now = crate::crew::now();
        Plan {
            id: Uuid::new_v4(),
            board: b.id,
            group: b.group.clone(),
            boss: "sarah".into(),
            goal: "Ship the new checkout".into(),
            state: PlanState::Proposed,
            paused: None,
            paused_why: None,
            parallel: Some(2),
            revision: 1,
            approved_revision: None,
            created_at: now,
            updated_at: now,
            result: None,
            agreed: None,
            agreed_at: None,
            read_back_at: None,
            goal_ticket: None,
            met: None,
        }
    }

    #[tokio::test]
    async fn a_plan_and_its_revisions_read_back_as_written() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let mut p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            assert_eq!(s.plan(p.id).await.unwrap().as_ref(), Some(&p));
            p.state = PlanState::Paused;
            p.paused = Some(Paused::BossHeld);
            p.approved_revision = Some(1);
            s.put_plan(&p).await.unwrap();
            assert_eq!(s.plan(p.id).await.unwrap().as_ref(), Some(&p));
            assert_eq!(
                s.plans(Some(PlanState::Paused)).await.unwrap(),
                vec![p.clone()]
            );
            assert!(s.plans(Some(PlanState::Running)).await.unwrap().is_empty());
            assert_eq!(s.plans(None).await.unwrap().len(), 1);

            let t = ticket(&s, &b, "Add the cart API").await;
            let r = Revision {
                plan: p.id,
                revision: 1,
                at: Utc::now(),
                by: Party::manager("sarah", "Sarah"),
                change: Change::Scope,
                summary: "Proposed".into(),
                snapshot: Snapshot {
                    result: None,
                    parallel: None,
                    budget: None,
                    goal: p.goal.clone(),
                    tickets: vec![Planned {
                        repo: None,
                        id: t.id,
                        title: t.title.clone(),
                        body: String::new(),
                        engine: Engine::Claude,
                        folder: "/work/acme-web".into(),
                        view: None,
                        needs: vec![],
                        machine: None,
                        model: None,
                        adopted: false,
                        session: None,
                    }],
                },
            };
            assert!(s.add_revision(&r).await.unwrap());
            assert!(
                !s.add_revision(&r).await.unwrap(),
                "a revision is written once"
            );
            let back = s.revision(p.id, 1).await.unwrap().unwrap();
            assert_eq!(back.snapshot, r.snapshot);
            assert_eq!(back.by, r.by);
            assert_eq!(s.revisions(p.id).await.unwrap().len(), 1);

            let runs = Runs {
                repo: None,
                plan: Some(p.id),
                engine: Some(Engine::Codex),
                folder: Some("/work/acme-api".into()),
                view: Some(View::Terminal),
                machine: Some("devbox".into()),
                model: Some("gpt-5.5".into()),
                adopted: false,
                held_back: None,
                session_closed: false,
            };
            s.put_runs(t.id, &runs, true).await.unwrap();
            assert_eq!(s.plan_runs(p.id).await.unwrap(), vec![(t.id, runs.clone())]);
            assert_eq!(s.plan_of_ticket(t.id).await.unwrap(), Some(p.id));

            // Left for its repository to say: the checkout a start found
            // stays, unless the plan had said a folder before.
            let left = Runs {
                folder: Some(String::new()),
                ..runs
            };
            let folder = async || s.plan_runs(p.id).await.unwrap()[0].1.folder.clone();
            s.put_runs(t.id, &left, true).await.unwrap();
            assert_eq!(folder().await.as_deref(), Some("/work/acme-api"));
            s.put_runs(t.id, &left, false).await.unwrap();
            assert_eq!(folder().await.as_deref(), Some(""));
        })
        .await;
    }

    /// A running plan with one ticket in it, read as it is now.
    async fn running(s: &DeskStore, b: &Board) -> (Plan, Ticket) {
        let mut p = new_plan(b);
        p.state = PlanState::Running;
        p.approved_revision = Some(1);
        s.put_plan(&p).await.unwrap();
        let t = ticket(s, b, "Add the cart API").await;
        s.put_runs(
            t.id,
            &Runs {
                repo: None,
                plan: Some(p.id),
                engine: Some(Engine::Claude),
                folder: Some("/work/acme-web".into()),
                view: None,
                machine: None,
                model: None,
                adopted: false,
                held_back: None,
                session_closed: false,
            },
            true,
        )
        .await
        .unwrap();
        let t = s.ticket(t.id).await.unwrap().unwrap();
        (p, t)
    }

    #[tokio::test]
    async fn a_start_moves_its_ticket_and_links_its_session_at_once_or_not_at_all() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let (p, mut t) = running(&s, &b).await;
            let read = t.clone();
            let session = Uuid::new_v4();
            let runs = s
                .begin_start(&mut t, p.id, 4096.0, session, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                runs.folder.as_deref(),
                Some("/work/acme-web"),
                "as the row says"
            );
            assert_eq!(t.state, TicketState::Doing);
            let now = s.ticket(t.id).await.unwrap().unwrap();
            assert_eq!(now.state, TicketState::Doing);
            assert!(
                now.links.is_empty(),
                "a starting link is no link of the board's"
            );
            let starting = s.starting().await.unwrap();
            assert_eq!(starting.len(), 1);
            assert_eq!((starting[0].ticket, starting[0].session), (t.id, session));
            assert_eq!(
                s.starting_of(t.id).await.unwrap().map(|x| x.session),
                Some(session)
            );

            // From an older read: nothing is written.
            let mut stale = read;
            let again = s
                .begin_start(&mut stale, p.id, 1.0, Uuid::new_v4(), Utc::now())
                .await;
            assert!(matches!(again, Err(BeginStart::Stale)));
            assert_eq!(s.starting().await.unwrap().len(), 1);

            s.end_start(t.id, session).await.unwrap();
            assert!(s.starting().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn a_ticket_moved_elsewhere_or_a_plan_paused_since_it_was_read_starts_nothing() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let (mut p, t) = running(&s, &b).await;
            // Where it runs changed after the runner read it.
            let mut read = t.clone();
            s.put_runs(
                t.id,
                &Runs {
                    repo: None,
                    plan: Some(p.id),
                    engine: Some(Engine::Claude),
                    folder: Some("/work/acme-api".into()),
                    view: None,
                    machine: None,
                    model: None,
                    adopted: false,
                    held_back: None,
                    session_closed: false,
                },
                true,
            )
            .await
            .unwrap();
            assert!(matches!(
                s.begin_start(&mut read, p.id, 1.0, Uuid::new_v4(), Utc::now())
                    .await,
                Err(BeginStart::Stale)
            ));
            // Paused meanwhile.
            let mut now = s.ticket(t.id).await.unwrap().unwrap();
            p.state = PlanState::Paused;
            p.paused = Some(Paused::Person);
            s.put_plan(&p).await.unwrap();
            assert!(matches!(
                s.begin_start(&mut now, p.id, 1.0, Uuid::new_v4(), Utc::now())
                    .await,
                Err(BeginStart::NotRunning)
            ));
            assert!(s.starting().await.unwrap().is_empty());
            assert_eq!(
                s.ticket(t.id).await.unwrap().unwrap().state,
                TicketState::Todo
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_new_plan_its_tickets_and_its_first_revision_are_written_together() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let p = new_plan(&b);
            let now = crate::crew::now();
            let mk = |title: &str, needs: Vec<Uuid>| Ticket {
                id: Uuid::new_v4(),
                board: b.id,
                number: 0,
                title: title.into(),
                body: String::new(),
                state: TicketState::Backlog,
                rank: RANK_STEP,
                assignee: None,
                needs,
                links: vec![],
                external: None,
                created_by: Party::manager("sarah", "Sarah"),
                pinned: false,
                snoozed_until: None,
                upstream_rank: None,
                snoozed_at: None,
                state_at: now,
                created_at: now,
                updated_at: now,
                version: 0,
            };
            let first = mk("a", vec![]);
            let second = mk("b", vec![first.id]);
            let runs = Runs {
                repo: None,
                plan: Some(p.id),
                engine: Some(Engine::Claude),
                folder: Some("/work".into()),
                view: None,
                machine: None,
                model: None,
                adopted: false,
                held_back: None,
                session_closed: false,
            };
            let mut rows = vec![(first, runs.clone()), (second, runs)];
            let r = Revision {
                plan: p.id,
                revision: 1,
                at: now,
                by: Party::manager("sarah", "Sarah"),
                change: Change::Scope,
                summary: "Proposed".into(),
                snapshot: Snapshot::default(),
            };
            s.add_plan(&p, &mut rows, &[], &r).await.unwrap();
            assert_eq!(rows[1].0.number, 2);
            assert_eq!(s.plan_runs(p.id).await.unwrap().len(), 2);
            let b2 = s.ticket(rows[1].0.id).await.unwrap().unwrap();
            assert_eq!(b2.needs, vec![rows[0].0.id]);
            assert!(s.revision(p.id, 1).await.unwrap().is_some());
            // The same again fails whole: nothing more is written.
            let mut again = vec![(mk("c", vec![]), Runs::default())];
            assert!(s.add_plan(&p, &mut again, &[], &r).await.is_err());
            assert!(s.ticket(again[0].0.id).await.unwrap().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn a_plan_goes_with_its_board_and_its_tickets_stay_when_it_does() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            let t = ticket(&s, &b, "Add the cart API").await;
            s.put_runs(
                t.id,
                &Runs {
                    plan: Some(p.id),
                    ..Default::default()
                },
                true,
            )
            .await
            .unwrap();
            let client = s.pool().get().await.unwrap();
            client
                .execute("DELETE FROM plans WHERE id = $1", &[&p.id])
                .await
                .unwrap();
            drop(client);
            assert_eq!(s.plan_of_ticket(t.id).await.unwrap(), None);
            assert!(s.ticket(t.id).await.unwrap().is_some());
            s.delete_board(b.id).await.unwrap();
            assert!(s.plans(None).await.unwrap().is_empty());
        })
        .await;
    }
}
