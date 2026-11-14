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
use crate::plan::{Change, Paused, Plan, PlanState, Revision};
use crate::store::DeskStore;

const PLAN: &str = "id, board, group_id, boss, goal, state, paused, parallel, revision, \
                    approved_revision, created_at, updated_at";
const REVISION: &str = "plan, revision, at, made_by, change, summary, snapshot";

/// The `ticket_links` kind of a session about to start for a ticket.
pub const STARTING: &str = "starting";

fn plan(row: &Row) -> Result<Plan> {
    let paused: Option<String> = row.try_get("paused")?;
    Ok(Plan {
        id: row.try_get("id")?,
        board: row.try_get("board")?,
        group: row.try_get("group_id")?,
        boss: row.try_get("boss")?,
        goal: row.try_get("goal")?,
        state: PlanState::parse(row.try_get("state")?)?,
        paused: paused.as_deref().map(Paused::parse).transpose()?,
        parallel: row.try_get("parallel")?,
        revision: row.try_get("revision")?,
        approved_revision: row.try_get("approved_revision")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
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

async fn put_plan_in<C: tokio_postgres::GenericClient>(client: &C, p: &Plan) -> Result<()> {
    client
        .execute(
            &format!(
                "INSERT INTO plans ({PLAN}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 ON CONFLICT (id) DO UPDATE SET board = EXCLUDED.board, group_id = EXCLUDED.group_id, \
                 boss = EXCLUDED.boss, goal = EXCLUDED.goal, state = EXCLUDED.state, \
                 paused = EXCLUDED.paused, parallel = EXCLUDED.parallel, \
                 revision = EXCLUDED.revision, approved_revision = EXCLUDED.approved_revision, \
                 updated_at = EXCLUDED.updated_at"
            ),
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
    /// says (its board `to`), and each of its tickets numbered on `to` in
    /// the order they were numbered, still in the plan, with what they
    /// need, their links, history and assignment kept. The tickets as they
    /// are now, in that order.
    pub async fn move_plan_board(&self, p: &Plan, to: Uuid) -> Result<Vec<Ticket>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let ids: Vec<Uuid> = tx
            .query(
                "SELECT id FROM tickets WHERE plan = $1 AND board <> $2 ORDER BY number",
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
    /// version moves on: a start read before is refused.
    pub async fn put_runs(&self, ticket: Uuid, runs: &Runs) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE tickets SET plan = $2, engine = $3, folder = $4, view = $5, \
                 version = version + 1 WHERE id = $1",
                &[
                    &ticket,
                    &runs.plan,
                    &runs.engine.map(Engine::as_str),
                    &runs.folder,
                    &runs.view.map(View::as_str),
                ],
            )
            .await?;
        Ok(())
    }

    /// Where each of `plan_id`'s tickets runs, by ticket.
    pub async fn plan_runs(&self, plan_id: Uuid) -> Result<Vec<(Uuid, Runs)>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, plan, engine, folder, view FROM tickets WHERE plan = $1",
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
                 version = version + 1 WHERE id = $1 AND version = $2 AND plan = $6 \
                 RETURNING version, engine, folder, view",
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
        })
    }

    /// Writes a new plan, its tickets (numbered on its board, with where
    /// each runs and what each needs) and its first revision, at once: a
    /// plan is never half there.
    pub async fn add_plan(
        &self,
        p: &Plan,
        tickets: &mut [(Ticket, Runs)],
        r: &Revision,
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        put_plan_in(&*tx, p).await?;
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
                "UPDATE tickets SET plan = $2, engine = $3, folder = $4, view = $5 WHERE id = $1",
                &[
                    &t.id,
                    &runs.plan,
                    &runs.engine.map(Engine::as_str),
                    &runs.folder,
                    &runs.view.map(View::as_str),
                ],
            )
            .await?;
        }
        for (t, _) in tickets.iter() {
            for n in &t.needs {
                tx.execute(
                    "INSERT INTO ticket_needs (ticket, needs) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    &[&t.id, n],
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
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::board::{Board, RANK_STEP};
    use crate::crew::Party;
    use crate::plan::{Planned, Snapshot};

    async fn board(s: &DeskStore) -> Board {
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

    async fn ticket(s: &DeskStore, b: &Board, title: &str) -> Ticket {
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
            snoozed_at: None,
            state_at: now,
            created_at: now,
            updated_at: now,
            version: 0,
        };
        s.add_ticket(&mut t).await.unwrap();
        t
    }

    fn new_plan(b: &Board) -> Plan {
        let now = crate::crew::now();
        Plan {
            id: Uuid::new_v4(),
            board: b.id,
            group: b.group.clone(),
            boss: "sarah".into(),
            goal: "Ship the new checkout".into(),
            state: PlanState::Proposed,
            paused: None,
            parallel: Some(2),
            revision: 1,
            approved_revision: None,
            created_at: now,
            updated_at: now,
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
                    parallel: None,
                    budget: None,
                    goal: p.goal.clone(),
                    tickets: vec![Planned {
                        id: t.id,
                        title: t.title.clone(),
                        body: String::new(),
                        engine: Engine::Claude,
                        folder: "/work/acme-web".into(),
                        view: None,
                        needs: vec![],
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
                plan: Some(p.id),
                engine: Some(Engine::Codex),
                folder: Some("/work/acme-api".into()),
                view: Some(View::Terminal),
            };
            s.put_runs(t.id, &runs).await.unwrap();
            assert_eq!(s.plan_runs(p.id).await.unwrap(), vec![(t.id, runs)]);
            assert_eq!(s.plan_of_ticket(t.id).await.unwrap(), Some(p.id));
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
                plan: Some(p.id),
                engine: Some(Engine::Claude),
                folder: Some("/work/acme-web".into()),
                view: None,
            },
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
                    plan: Some(p.id),
                    engine: Some(Engine::Claude),
                    folder: Some("/work/acme-api".into()),
                    view: None,
                },
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
                snoozed_at: None,
                state_at: now,
                created_at: now,
                updated_at: now,
                version: 0,
            };
            let first = mk("a", vec![]);
            let second = mk("b", vec![first.id]);
            let runs = Runs {
                plan: Some(p.id),
                engine: Some(Engine::Claude),
                folder: Some("/work".into()),
                view: None,
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
            s.add_plan(&p, &mut rows, &r).await.unwrap();
            assert_eq!(rows[1].0.number, 2);
            assert_eq!(s.plan_runs(p.id).await.unwrap().len(), 2);
            let b2 = s.ticket(rows[1].0.id).await.unwrap().unwrap();
            assert_eq!(b2.needs, vec![rows[0].0.id]);
            assert!(s.revision(p.id, 1).await.unwrap().is_some());
            // The same again fails whole: nothing more is written.
            let mut again = vec![(mk("c", vec![]), Runs::default())];
            assert!(s.add_plan(&p, &mut again, &r).await.is_err());
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
