//! A boss's assignments in Postgres (`boss_assignments`;
//! [`crate::assignment`]). A new one is checked against the boss's live
//! ones and reserved in one short transaction under the boss's advisory
//! lock, so two orders sent at once for one job can't both pass; a partial
//! unique index on its live claims holds even if the lock is bypassed.
//! Whether an order went in is read from its line message in the same
//! transaction, never only from what was last heard of it.

use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::assignment::{self, Assignment, AssignmentState, Live, Rule, Ruling};
use crate::store::DeskStore;

const COLUMNS: &str = "a.id, a.boss, a.session, a.machine, a.line_message, a.text, a.job, \
    a.job_key, a.repo, a.targets, a.irreversible, a.urgent, a.claims, a.state, a.sent_at, \
    a.delivered_at, a.acked_at, a.closed_at, a.closed_by, a.closed_why, a.replaces, a.turn, \
    a.item, a.stand_down, a.answer, a.action, a.basis";

/// The live assignments, each with whether its line message went in.
const LIVE: &str = "('sending', 'open', 'held')";

/// When the line message carrying it went in, if it did: as the line
/// row says (taken by the session's hook counts).
const WENT_IN: &str = "CASE WHEN l.delivery IN ('delivered', 'handed') \
    THEN COALESCE(l.delivered_at, l.updated_at) END";

fn assignment(r: &Row) -> Result<Assignment> {
    let state: String = r.try_get(13)?;
    let went_in: Option<DateTime<Utc>> = r.try_get(27)?;
    let delivered_at: Option<DateTime<Utc>> = r.try_get(15)?;
    Ok(Assignment {
        id: r.try_get(0)?,
        boss: r.try_get(1)?,
        session: r.try_get(2)?,
        machine: r.try_get(3)?,
        line_message: r.try_get(4)?,
        text: r.try_get(5)?,
        job: r.try_get(6)?,
        job_key: r.try_get(7)?,
        repo: r.try_get(8)?,
        targets: r.try_get(9)?,
        irreversible: r.try_get(10)?,
        urgent: r.try_get(11)?,
        claims: r.try_get(12)?,
        state: AssignmentState::parse(&state)?,
        sent_at: r.try_get(14)?,
        delivered_at: delivered_at.or(went_in),
        acked_at: r.try_get(16)?,
        closed_at: r.try_get(17)?,
        closed_by: r.try_get(18)?,
        closed_why: r.try_get(19)?,
        replaces: r.try_get(20)?,
        turn: r.try_get(21)?,
        item: r.try_get(22)?,
        stand_down: r.try_get(23)?,
        answer: r.try_get(24)?,
        action: r.try_get(25)?,
        basis: r.try_get(26)?,
    })
}

/// A select of assignments with what their line rows say, after `rest`.
fn select(rest: &str) -> String {
    format!(
        "SELECT {COLUMNS}, {WENT_IN} FROM boss_assignments a \
         LEFT JOIN line_messages l ON l.id = a.line_message {rest}"
    )
}

/// Whether `a` went in: its line message did, or, with none here (a
/// machine's session), it was sent there.
fn went_in(a: &Assignment) -> bool {
    a.delivered_at.is_some() || (a.line_message.is_none() && a.state == AssignmentState::Open)
}

/// What came of reserving an assignment.
#[derive(Debug, Clone)]
pub enum Reserved {
    /// Kept, as `sending` (or `held`): send it now. The same session's
    /// orders it superseded, and whether it is a word on a job that
    /// session already holds.
    Kept {
        superseded: Vec<Assignment>,
        follow_up: bool,
    },
    /// Not kept: `rule` refuses it, for the live assignment `holder`.
    Refused { rule: Rule, holder: Box<Assignment> },
}

impl DeskStore {
    /// Checks `new` against `boss`'s live assignments and keeps it, in one
    /// transaction under the boss's lock: R1–R3 ([`assignment::rule`]),
    /// superseding the same session's earlier order for the job, and with
    /// `replacing`, cancelling the assignment it takes over (and why).
    pub async fn assignment_reserve(
        &self,
        new: &Assignment,
        replacing: Option<(Uuid, &str)>,
    ) -> Result<Reserved> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // int4 collisions between bosses only make them wait on each other.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('boss_assignments:' || $1))",
            &[&new.boss],
        )
        .await?;
        let live: Vec<Live> = tx
            .query(
                &select(&format!("WHERE a.boss = $1 AND a.state IN {LIVE}")),
                &[&new.boss],
            )
            .await?
            .iter()
            .map(assignment)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|a| Live {
                delivered: went_in(&a),
                a,
            })
            .collect();
        let (supersede, follow_up) = match assignment::rule(new, &live, replacing.map(|(id, _)| id))
        {
            Ruling::Refused { rule, holder } => {
                let holder = live
                    .into_iter()
                    .find(|l| l.a.id == holder)
                    .map(|l| Box::new(l.a))
                    .ok_or_else(|| eyre::eyre!("the holder went missing"))?;
                return Ok(Reserved::Refused { rule, holder });
            }
            Ruling::Send {
                supersede,
                follow_up,
            } => (supersede, follow_up),
        };
        let now = crate::crew::now();
        let why = format!("superseded by {}", assignment::short(new.id));
        let mut superseded = Vec::new();
        for id in &supersede {
            if let Some(r) = tx
                .query_opt(
                    &format!(
                        "UPDATE boss_assignments a SET state = 'superseded', closed_at = $2, \
                         closed_by = 'boss', closed_why = $3 \
                         WHERE id = $1 AND state IN {LIVE} RETURNING {COLUMNS}, NULL::timestamptz"
                    ),
                    &[id, &now, &why],
                )
                .await?
            {
                superseded.push(assignment(&r)?);
            }
        }
        if let Some((old, why)) = replacing {
            tx.execute(
                &format!(
                    "UPDATE boss_assignments SET state = 'cancelled', closed_at = $2, \
                     closed_by = 'boss', closed_why = $3 WHERE id = $1 AND state IN {LIVE}"
                ),
                &[&old, &now, &why],
            )
            .await?;
        }
        let claims = new.claims && !follow_up;
        let inserted = tx
            .execute(
                "INSERT INTO boss_assignments (id, boss, session, machine, line_message, text, job, \
                 job_key, repo, targets, irreversible, urgent, claims, state, sent_at, replaces, \
                 turn, item, stand_down, action, basis) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 $17, $18, $19, $20, $21)",
                &[
                    &new.id,
                    &new.boss,
                    &new.session,
                    &new.machine,
                    &new.line_message,
                    &new.text,
                    &new.job,
                    &new.job_key,
                    &new.repo,
                    &new.targets,
                    &new.irreversible,
                    &new.urgent,
                    &claims,
                    &new.state.as_str(),
                    &new.sent_at,
                    &new.replaces,
                    &new.turn,
                    &new.item,
                    &new.stand_down,
                    &new.action,
                    &new.basis,
                ],
            )
            .await;
        if let Err(e) = inserted {
            // Another claim on the job reached the table past the lock: the
            // index refuses it as R2 would.
            if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) {
                drop(tx);
                let holder = self
                    .assignments_of(&new.boss, true)
                    .await?
                    .into_iter()
                    .find(|a| a.claims && a.job_key == new.job_key);
                if let Some(holder) = holder {
                    return Ok(Reserved::Refused {
                        rule: Rule::SameJob,
                        holder: Box::new(holder),
                    });
                }
            }
            return Err(e.into());
        }
        tx.commit().await?;
        Ok(Reserved::Kept {
            superseded,
            follow_up,
        })
    }

    /// One assignment, with what its line row says.
    pub async fn assignment(&self, id: Uuid) -> Result<Option<Assignment>> {
        let client = self.pool().get().await?;
        client
            .query_opt(&select("WHERE a.id = $1"), &[&id])
            .await?
            .as_ref()
            .map(assignment)
            .transpose()
    }

    /// `boss`'s assignments, newest first: only the live ones with
    /// `live_only`, else those of the last day too.
    pub async fn assignments_of(&self, boss: &str, live_only: bool) -> Result<Vec<Assignment>> {
        let client = self.pool().get().await?;
        let since = Utc::now() - chrono::TimeDelta::days(1);
        client
            .query(
                &select(&format!(
                    "WHERE a.boss = $1 AND (a.state IN {LIVE} OR (NOT $2 AND a.sent_at >= $3)) \
                     ORDER BY a.sent_at DESC LIMIT 200"
                )),
                &[&boss, &live_only, &since],
            )
            .await?
            .iter()
            .map(assignment)
            .collect()
    }

    /// Every live assignment of every boss, oldest first.
    pub async fn assignments_live(&self) -> Result<Vec<Assignment>> {
        let client = self.pool().get().await?;
        client
            .query(
                &select(&format!("WHERE a.state IN {LIVE} ORDER BY a.sent_at")),
                &[],
            )
            .await?
            .iter()
            .map(assignment)
            .collect()
    }

    /// The reserved assignment `id` was sent (`open`, carried by
    /// `line_message`) or wasn't (`failed`, which holds no job). None when
    /// it was no longer being sent.
    pub async fn assignment_sent(
        &self,
        id: Uuid,
        line_message: Option<Uuid>,
        ok: bool,
    ) -> Result<Option<Assignment>> {
        let client = self.pool().get().await?;
        let (state, why): (&str, Option<&str>) = match ok {
            true => ("open", None),
            false => ("failed", Some("it wasn't sent")),
        };
        client
            .query_opt(
                &format!(
                    "UPDATE boss_assignments a SET state = $2, line_message = COALESCE($3, line_message), \
                     closed_at = CASE WHEN $2 = 'failed' THEN now() END, closed_why = $4 \
                     WHERE id = $1 AND state = 'sending' RETURNING {COLUMNS}, NULL::timestamptz"
                ),
                &[&id, &state, &line_message, &why],
            )
            .await?
            .as_ref()
            .map(assignment)
            .transpose()
    }

    /// An order superseded by one that then couldn't go (its line message
    /// had gone in meanwhile) stands again.
    pub async fn assignment_reopen(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_assignments SET state = 'open', closed_at = NULL, closed_by = NULL, \
                 closed_why = NULL WHERE id = $1 AND state = 'superseded'",
                &[&id],
            )
            .await?;
        Ok(())
    }

    /// Closes the live assignment `id` as `state` (done, cancelled, lapsed,
    /// failed), by `by` and why. The row as it now is; none when it wasn't
    /// live.
    pub async fn assignment_close(
        &self,
        id: Uuid,
        state: AssignmentState,
        by: &str,
        why: &str,
    ) -> Result<Option<Assignment>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_assignments a SET state = $2, closed_at = now(), closed_by = $3, \
                     closed_why = $4 WHERE id = $1 AND state IN {LIVE} \
                     RETURNING {COLUMNS}, NULL::timestamptz"
                ),
                &[&id, &state.as_str(), &by, &why],
            )
            .await?
            .as_ref()
            .map(assignment)
            .transpose()
    }

    /// The live assignment carried by `line_message`, if any.
    pub async fn assignment_of_line(&self, line_message: Uuid) -> Result<Option<Assignment>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &select(&format!(
                    "WHERE a.line_message = $1 AND a.state IN {LIVE} LIMIT 1"
                )),
                &[&line_message],
            )
            .await?
            .as_ref()
            .map(assignment)
            .transpose()
    }

    /// Its line message went in at `at`.
    pub async fn assignment_delivered(&self, line_message: Uuid, at: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_assignments SET delivered_at = $2 \
                 WHERE line_message = $1 AND delivered_at IS NULL",
                &[&line_message, &at],
            )
            .await?;
        Ok(())
    }

    /// Its session answered it at `at`. True when this was its first answer.
    pub async fn assignment_acked(&self, id: Uuid, at: DateTime<Utc>) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE boss_assignments SET acked_at = $2 WHERE id = $1 AND acked_at IS NULL",
                &[&id, &at],
            )
            .await?
            > 0)
    }

    /// The held assignment `id` goes out now, its old holder's `answer`
    /// kept: back to `sending`. None when it was no longer held.
    pub async fn assignment_release(
        &self,
        id: Uuid,
        answer: Option<&str>,
    ) -> Result<Option<Assignment>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_assignments a SET state = 'sending', answer = COALESCE($2, answer) \
                     WHERE id = $1 AND state = 'held' RETURNING {COLUMNS}, NULL::timestamptz"
                ),
                &[&id, &answer],
            )
            .await?
            .as_ref()
            .map(assignment)
            .transpose()
    }

    /// The held assignment `id` waits on the answer to `stand_down`, the
    /// line message telling its old holder to stand down.
    pub async fn assignment_wait_on(&self, id: Uuid, stand_down: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_assignments SET stand_down = $2 WHERE id = $1",
                &[&id, &stand_down],
            )
            .await?;
        Ok(())
    }

    /// The person's order `item` is what `boss`'s turn on runner session
    /// `sid` (its turn keys start `<sid>#`) served, since `since`.
    pub async fn assignments_serve_item(
        &self,
        boss: &str,
        sid: &str,
        since: DateTime<Utc>,
        item: Uuid,
    ) -> Result<u64> {
        let client = self.pool().get().await?;
        let like = format!("{}#%", sid.replace('%', "\\%").replace('_', "\\_"));
        Ok(client
            .execute(
                "UPDATE boss_assignments SET item = $4 WHERE boss = $1 AND turn LIKE $2 \
                 AND sent_at >= $3 AND item IS NULL",
                &[&boss, &like, &since, &item],
            )
            .await?)
    }

    /// After a restart: what was still being sent before `before` is
    /// settled. One whose line message is there (its boss's, to its
    /// session, its words, sent after it) is `open`; any other `failed`.
    /// Each as it now is.
    pub async fn assignments_reconcile(&self, before: DateTime<Utc>) -> Result<Vec<Assignment>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "WITH found AS ( \
                       SELECT a.id, ( \
                         SELECT l.id FROM line_messages l \
                         WHERE l.sender ->> 'id' = a.boss AND l.recipient ->> 'id' = a.session::text \
                         AND starts_with(l.text, a.text) AND l.at >= a.sent_at - interval '1 second' \
                         ORDER BY l.at LIMIT 1) AS line \
                       FROM boss_assignments a WHERE a.state = 'sending' AND a.sent_at < $1) \
                     UPDATE boss_assignments a SET \
                       state = CASE WHEN f.line IS NULL THEN 'failed' ELSE 'open' END, \
                       line_message = COALESCE(f.line, a.line_message), \
                       closed_at = CASE WHEN f.line IS NULL THEN now() END, \
                       closed_why = CASE WHEN f.line IS NULL THEN 'Desk restarted before it was sent' END \
                     FROM found f WHERE a.id = f.id RETURNING {COLUMNS}, NULL::timestamptz"
                ),
                &[&before],
            )
            .await?;
        rows.iter().map(assignment).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::assignment::{read, Job, Target, TargetKind};

    /// The boss's order `text` to `session`, declaring the release it names.
    fn order(boss: &str, session: Uuid, text: &str) -> Assignment {
        let r = read(text, false);
        let mut a = Assignment::new(boss, session, text);
        let release = r
            .targets
            .iter()
            .find(|t| t.kind == TargetKind::Release)
            .map(|t| t.id.clone());
        a.job_key = release.map(|k| Job::new("release", "acme/harbor", &k).unwrap().as_key());
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.claims = a.job_key.is_some();
        a.repo = Some("acme/harbor".into());
        a
    }

    #[tokio::test]
    async fn two_senders_at_once_for_one_job_only_one_passes() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            for _ in 0..5 {
                let boss = format!("boss-{}", Uuid::new_v4().simple());
                let go = Arc::new(tokio::sync::Barrier::new(2));
                let send = |text: &'static str| {
                    let (store, go, boss) = (Arc::clone(&store), Arc::clone(&go), boss.clone());
                    tokio::spawn(async move {
                        let a = order(&boss, Uuid::new_v4(), text);
                        go.wait().await;
                        store.assignment_reserve(&a, None).await.unwrap()
                    })
                };
                let one = send("You now own release 4, and only you dispatch it.");
                let two = send("URGENT, one job: start Harbor production release 4.");
                let (one, two) = (one.await.unwrap(), two.await.unwrap());
                let kept = [&one, &two]
                    .iter()
                    .filter(|r| matches!(r, Reserved::Kept { .. }))
                    .count();
                assert_eq!(kept, 1, "{one:?} {two:?}");
                assert_eq!(store.assignments_of(&boss, true).await.unwrap().len(), 1);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn the_index_holds_one_claim_on_a_job_even_past_the_lock() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let first = order("boss", Uuid::new_v4(), "Dispatch release 4.");
            store.assignment_reserve(&first, None).await.unwrap();
            let second = order("boss", Uuid::new_v4(), "Dispatch release 4 now.");
            let client = store.pool().get().await.unwrap();
            let raw = client
                .execute(
                    "INSERT INTO boss_assignments (id, boss, session, text, job_key, claims, state, sent_at) \
                     VALUES ($1, 'boss', $2, 'x', $3, true, 'open', now())",
                    &[&second.id, &second.session, &second.job_key],
                )
                .await;
            assert!(raw.is_err(), "a second live claim is refused by the table");
        })
        .await;
    }

    #[tokio::test]
    async fn what_a_restart_left_being_sent_is_open_when_its_message_went_and_failed_when_not() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let (s, t) = (Uuid::new_v4(), Uuid::new_v4());
            let mut sent = order("boss", s, "Dispatch release 4.");
            sent.sent_at = Utc::now() - chrono::TimeDelta::minutes(5);
            let mut lost = order("boss", t, "Merge #711.");
            lost.sent_at = Utc::now() - chrono::TimeDelta::minutes(5);
            let mut fresh = order("boss", t, "Now run the linter.");
            fresh.sent_at = Utc::now();
            for a in [&sent, &lost, &fresh] {
                assert!(matches!(
                    store.assignment_reserve(a, None).await.unwrap(),
                    Reserved::Kept { .. }
                ));
            }
            // Its line message went in before the restart.
            let mut m = crate::line::LineMessage::new(
                crate::crew::Party::manager("boss", "Desk"),
                crate::crew::Party {
                    kind: crate::crew::PartyKind::Session,
                    id: s.to_string(),
                    name: "Second batch prep".into(),
                    engine: None,
                    origin: None,
                },
                crate::line::LineKind::Tell,
                "Dispatch release 4.".into(),
                1,
            );
            m.at = sent.sent_at + chrono::TimeDelta::seconds(1);
            store.put_line(&m).await.unwrap();
            let settled = store
                .assignments_reconcile(Utc::now() - chrono::TimeDelta::minutes(2))
                .await
                .unwrap();
            assert_eq!(settled.len(), 2, "only what is older than two minutes");
            let now = |id| {
                let store = &store;
                async move { store.assignment(id).await.unwrap().unwrap() }
            };
            let a = now(sent.id).await;
            assert_eq!(
                (a.state, a.line_message),
                (AssignmentState::Open, Some(m.id))
            );
            assert_eq!(now(lost.id).await.state, AssignmentState::Failed);
            assert_eq!(now(fresh.id).await.state, AssignmentState::Sending);
            // Failed holds no job: it may be given again.
            let again = order("boss", Uuid::new_v4(), "Merge #711.");
            assert!(matches!(
                store.assignment_reserve(&again, None).await.unwrap(),
                Reserved::Kept { .. }
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn delivery_is_read_from_the_line_row() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let s = Uuid::new_v4();
            let first = order("boss", s, "Dispatch the real release 3 promote now.");
            store.assignment_reserve(&first, None).await.unwrap();
            let m = crate::line::LineMessage::new(
                crate::crew::Party::manager("boss", "Desk"),
                crate::crew::Party::you(),
                crate::line::LineKind::Tell,
                first.text.clone(),
                1,
            );
            store.put_line(&m).await.unwrap();
            store
                .assignment_sent(first.id, Some(m.id), true)
                .await
                .unwrap();
            // Still queued: a new order takes its place.
            let again = order("boss", s, "Start Harbor production release 3 now.");
            match store.assignment_reserve(&again, None).await.unwrap() {
                Reserved::Kept { superseded, .. } => assert_eq!(superseded[0].id, first.id),
                other => panic!("{other:?}"),
            }
            // As the hub puts it back when the old one went in meanwhile.
            store
                .assignment_close(again.id, AssignmentState::Failed, "desk", "test")
                .await
                .unwrap();
            store.assignment_reopen(first.id).await.unwrap();
            // Gone in: the same order again is refused.
            store
                .line_take(m.id, crate::line::Delivery::Delivered)
                .await
                .unwrap();
            let third = order("boss", s, "Start Harbor production release 3 now.");
            assert!(matches!(
                store.assignment_reserve(&third, None).await.unwrap(),
                Reserved::Refused {
                    rule: Rule::SameSession,
                    ..
                }
            ));
        })
        .await;
    }
}
