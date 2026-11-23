//! Goals in Postgres: a plan's `plan_readings`, each read of a measured
//! part, and its `plan_facts`, what its boss reported or showed toward it.
//! When a goal was met and said back are written here, each once.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::goal::{Fact, FactKind, GoalSaid, Heads, Met, Reading};
use crate::plan::Plan;
use crate::plan_store::PLAN;
use crate::store::DeskStore;

/// The newest readings kept of each part; older ones are kept only when
/// they came nearer the target than every reading before them.
pub const READINGS_KEPT: i64 = 200;

const READING: &str = "at, part, value, met, words, evidence, failed";
const FACT: &str = "id, plan, kind, part, at, text, evidence, checked, made_by, replaces";

fn part_of(n: i16) -> u16 {
    u16::try_from(n).unwrap_or_default()
}

fn reading(row: &Row) -> Result<Reading> {
    Ok(Reading {
        at: row.try_get("at")?,
        part: part_of(row.try_get("part")?),
        value: row.try_get("value")?,
        met: row.try_get("met")?,
        words: row.try_get("words")?,
        evidence: serde_json::from_value(row.try_get("evidence")?)
            .context("reading a reading's evidence")?,
        failed: row.try_get("failed")?,
    })
}

fn fact(row: &Row) -> Result<Fact> {
    Ok(Fact {
        id: row.try_get("id")?,
        plan: row.try_get("plan")?,
        kind: FactKind::parse(row.try_get("kind")?)?,
        part: row.try_get::<_, Option<i16>>("part")?.map(part_of),
        at: row.try_get("at")?,
        text: row.try_get("text")?,
        evidence: serde_json::from_value(row.try_get("evidence")?)
            .context("reading a fact's evidence")?,
        checked: row.try_get("checked")?,
        made_by: serde_json::from_value(row.try_get("made_by")?)
            .context("reading who reported a fact")?,
        replaces: row.try_get("replaces")?,
    })
}

impl DeskStore {
    /// Keeps `r` of plan `plan`, and lets go of what of its part is no
    /// longer kept: every reading past the newest [`READINGS_KEPT`] but
    /// those that came nearer the target than all before them, a lower
    /// value being nearer when `lower_is_nearer`.
    pub async fn keep_reading(&self, plan: Uuid, r: &Reading, lower_is_nearer: bool) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let part = i16::try_from(r.part).context("a reading's part")?;
        tx.execute(
            &format!(
                "INSERT INTO plan_readings (plan, {READING}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (plan, at, part) DO NOTHING"
            ),
            &[
                &plan,
                &r.at,
                &part,
                &r.value,
                &r.met,
                &r.words,
                &serde_json::to_value(&r.evidence)?,
                &r.failed,
            ],
        )
        .await?;
        tx.execute(
            "WITH ranked AS ( \
               SELECT at, value, \
                 row_number() OVER (ORDER BY at DESC) AS newest, \
                 CASE WHEN $4 \
                   THEN min(value) OVER (ORDER BY at ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) \
                   ELSE max(value) OVER (ORDER BY at ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) \
                 END AS best_before \
               FROM plan_readings WHERE plan = $1 AND part = $2) \
             DELETE FROM plan_readings r USING ranked k \
             WHERE r.plan = $1 AND r.part = $2 AND r.at = k.at AND k.newest > $3 \
               AND NOT (k.value IS NOT NULL AND (k.best_before IS NULL \
                 OR CASE WHEN $4 THEN k.value < k.best_before ELSE k.value > k.best_before END))",
            &[&plan, &part, &READINGS_KEPT, &lower_is_nearer],
        )
        .await?;
        tx.commit().await.context("keeping a goal's reading")?;
        Ok(())
    }

    /// Plan `plan`'s readings, oldest first.
    pub async fn readings(&self, plan: Uuid) -> Result<Vec<Reading>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {READING} FROM plan_readings WHERE plan = $1 ORDER BY at, part"),
                &[&plan],
            )
            .await?
            .iter()
            .map(reading)
            .collect()
    }

    /// Keeps `f`.
    pub async fn keep_fact(&self, f: &Fact) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO plan_facts ({FACT}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
                ),
                &[
                    &f.id,
                    &f.plan,
                    &f.kind.as_str(),
                    &f.part.map(i16::try_from).transpose()?,
                    &f.at,
                    &f.text,
                    &serde_json::to_value(&f.evidence)?,
                    &f.checked,
                    &serde_json::to_value(&f.made_by)?,
                    &f.replaces,
                ],
            )
            .await?;
        Ok(())
    }

    /// The facts and checkpoints of `plans` still shown, newest first: one
    /// another corrects is not.
    pub async fn facts(&self, plans: &[Uuid]) -> Result<Vec<Fact>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {FACT} FROM plan_facts f WHERE plan = ANY($1) \
                     AND NOT EXISTS (SELECT 1 FROM plan_facts n WHERE n.replaces = f.id) \
                     ORDER BY at DESC, id"
                ),
                &[&plans],
            )
            .await?
            .iter()
            .map(fact)
            .collect()
    }

    /// Marks plan `plan`'s goal met as `met` says, unless it was met
    /// already: whether this did.
    pub async fn mark_met(&self, plan: Uuid, met: &Met) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE plans SET met_at = $2, met_by = $3, met_why = $4, updated_at = $2 \
                 WHERE id = $1 AND result IS NOT NULL AND met_at IS NULL",
                &[&plan, &met.at, &met.by.as_str(), &met.why],
            )
            .await?;
        Ok(n == 1)
    }

    /// What plan `plan`'s boss was told of its goal by the hub's checks:
    /// nothing for a plan there isn't.
    pub async fn goal_said(&self, plan: Uuid) -> Result<GoalSaid> {
        self.plan_kept(plan, "goal_said").await
    }

    /// Keeps `said` as what plan `plan`'s boss was told of its goal.
    pub async fn keep_goal_said(&self, plan: Uuid, said: &GoalSaid) -> Result<()> {
        self.keep_plan_kept(plan, "goal_said", said).await
    }

    /// Where plan `plan`'s goal's branches and pull requests stood when it
    /// was agreed or a checkpoint last moved it: none when not yet kept.
    pub async fn goal_heads(&self, plan: Uuid) -> Result<Heads> {
        self.plan_kept(plan, "goal_heads").await
    }

    /// Keeps `heads` as where plan `plan`'s goal's work stands now.
    pub async fn keep_goal_heads(&self, plan: Uuid, heads: &Heads) -> Result<()> {
        self.keep_plan_kept(plan, "goal_heads", heads).await
    }

    /// What the goal checks keep in plan `plan`'s `column`: its default
    /// when there is no such plan.
    async fn plan_kept<T: serde::de::DeserializeOwned + Default>(
        &self,
        plan: Uuid,
        column: &'static str,
    ) -> Result<T> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                &format!("SELECT {column} FROM plans WHERE id = $1"),
                &[&plan],
            )
            .await?;
        Ok(match row {
            Some(r) => serde_json::from_value(r.try_get(0)?)
                .with_context(|| format!("a plan's {column}"))?,
            None => T::default(),
        })
    }

    /// Keeps `value` in plan `plan`'s `column`.
    async fn keep_plan_kept<T: serde::Serialize>(
        &self,
        plan: Uuid,
        column: &'static str,
        value: &T,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!("UPDATE plans SET {column} = $2 WHERE id = $1"),
                &[&plan, &serde_json::to_value(value)?],
            )
            .await?;
        Ok(())
    }

    /// Notes that plan `plan`'s goal was said back to the person at `at`.
    pub async fn set_read_back(&self, plan: Uuid, at: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE plans SET read_back_at = $2 WHERE id = $1",
                &[&plan, &at],
            )
            .await?;
        Ok(())
    }

    /// The goals not yet met of plans that haven't ended, newest first;
    /// only `boss`'s when it is said.
    pub async fn open_goals(&self, boss: Option<&str>) -> Result<Vec<Plan>> {
        self.goals_where("($1::text IS NULL OR boss = $1)", &[&boss])
            .await
    }

    /// The agreed goal not yet met, of a plan running or paused, that
    /// `ticket` stands for: a draft's ticket is no goal's yet.
    pub async fn open_goal_of_ticket(&self, ticket: Uuid) -> Result<Option<Plan>> {
        Ok(self
            .goals_where(
                "goal_ticket = $1 AND state IN ('running', 'paused') \
                 AND approved_revision IS NOT NULL",
                &[&ticket],
            )
            .await?
            .into_iter()
            .next())
    }

    async fn goals_where(
        &self,
        filter: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<Plan>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {PLAN} FROM plans WHERE result IS NOT NULL AND met_at IS NULL \
                     AND state IN ('proposed', 'running', 'paused') \
                     AND {filter} ORDER BY created_at DESC, id"
                ),
                params,
            )
            .await?
            .iter()
            .map(crate::plan_store::plan)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::crew::Party;
    use crate::goal::{Evidence, GoalResult, Measure, MetBy};
    use crate::plan::PlanState;
    use crate::plan_store::tests::{board, new_plan, ticket};

    fn read(at: DateTime<Utc>, value: Option<f64>) -> Reading {
        Reading {
            at,
            part: 0,
            value,
            met: false,
            words: "read".into(),
            evidence: vec![Evidence::Run {
                repo: "acme/api".into(),
                id: 7,
            }],
            failed: value.is_none().then(|| "slow".to_string()),
        }
    }

    fn goal(p: &mut Plan, ticket: Uuid) {
        p.result = Some(GoalResult {
            words: "CI under 5 minutes".into(),
            parts: vec![Measure::Judged {
                evidence: "the work".into(),
            }],
        });
        p.agreed = Some("yes do it".into());
        p.agreed_at = Some(crate::crew::now());
        p.goal_ticket = Some(ticket);
    }

    #[tokio::test]
    async fn a_goal_s_columns_read_back_as_written_and_are_met_once() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let t = ticket(&s, &b, "Speed up CI").await;
            let mut p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            assert!(s.open_goals(None).await.unwrap().is_empty());
            goal(&mut p, t.id);
            s.put_plan(&p).await.unwrap();
            assert_eq!(s.plan(p.id).await.unwrap().as_ref(), Some(&p));
            assert_eq!(s.open_goals(Some("sarah")).await.unwrap(), vec![p.clone()]);
            assert!(s.open_goals(Some("other")).await.unwrap().is_empty());
            assert_eq!(
                s.open_goal_of_ticket(t.id).await.unwrap(),
                None,
                "a draft's ticket is no goal's yet"
            );
            p.state = PlanState::Running;
            p.approved_revision = Some(p.revision);
            s.put_plan(&p).await.unwrap();
            assert_eq!(s.open_goal_of_ticket(t.id).await.unwrap(), Some(p.clone()));
            assert_eq!(s.open_goal_of_ticket(Uuid::new_v4()).await.unwrap(), None);

            let said = crate::crew::now();
            s.set_read_back(p.id, said).await.unwrap();
            let met = Met {
                at: said,
                by: MetBy::Person,
                why: "the person said so".into(),
            };
            let (a, b2) = tokio::join!(s.mark_met(p.id, &met), s.mark_met(p.id, &met));
            assert_eq!(
                [a.unwrap(), b2.unwrap()].iter().filter(|m| **m).count(),
                1,
                "met once"
            );
            let back = s.plan(p.id).await.unwrap().unwrap();
            assert_eq!(back.met, Some(met.clone()));
            assert_eq!(back.read_back_at, Some(said));
            s.put_plan(&p).await.unwrap();
            let kept = s.plan(p.id).await.unwrap().unwrap();
            assert!(
                kept.met.is_some(),
                "a plan written from an older read keeps it met"
            );
            assert_eq!(kept.read_back_at, Some(said));
            assert!(s.open_goals(None).await.unwrap().is_empty());
            assert_eq!(s.open_goal_of_ticket(t.id).await.unwrap(), None, "met");

            p.state = PlanState::Dropped;
            s.put_plan(&p).await.unwrap();
            assert!(
                !s.mark_met(new_plan(&b).id, &met).await.unwrap(),
                "no such plan"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn what_a_goal_s_boss_was_told_is_kept_with_its_plan() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            assert_eq!(s.goal_said(p.id).await.unwrap(), GoalSaid::default());
            let at = crate::crew::now();
            let mut said = GoalSaid::default();
            said.spells.insert(1, (at, 2));
            said.failing.insert(0, at);
            said.undelivered_told(Uuid::new_v4());
            said.noted = Some((at, 2));
            s.keep_goal_said(p.id, &said).await.unwrap();
            assert_eq!(s.goal_said(p.id).await.unwrap(), said);
            s.put_plan(&p).await.unwrap();
            assert_eq!(
                s.goal_said(p.id).await.unwrap(),
                said,
                "a plan written again keeps it"
            );
            assert_eq!(
                s.goal_said(Uuid::new_v4()).await.unwrap(),
                GoalSaid::default()
            );
            assert!(s.goal_heads(p.id).await.unwrap().is_empty());
            let heads = Heads::from([(crate::goal::head_key("Acme/Web", "main"), "a".repeat(40))]);
            s.keep_goal_heads(p.id, &heads).await.unwrap();
            assert_eq!(s.goal_heads(p.id).await.unwrap(), heads);
            assert_eq!(s.goal_said(p.id).await.unwrap(), said, "each kept apart");
        })
        .await;
    }

    #[tokio::test]
    async fn readings_keep_the_newest_and_every_one_that_came_nearer() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            let start = crate::crew::now() - Duration::days(1);
            let n = READINGS_KEPT + 20;
            for i in 0..n {
                let value = match i {
                    0 => Some(30.0),
                    5 => Some(20.0),
                    9 => None,
                    _ => Some(40.0),
                };
                s.keep_reading(p.id, &read(start + Duration::minutes(i), value), true)
                    .await
                    .unwrap();
            }
            let kept = s.readings(p.id).await.unwrap();
            assert_eq!(kept.len() as i64, READINGS_KEPT + 2);
            assert_eq!(kept[0].value, Some(30.0));
            assert_eq!(kept[1].value, Some(20.0));
            assert_eq!(kept[2].at, start + Duration::minutes(n - READINGS_KEPT));
            assert_eq!(kept[0].evidence, read(start, None).evidence);
        })
        .await;
    }

    #[tokio::test]
    async fn a_corrected_fact_is_no_longer_shown() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let b = board(&s).await;
            let p = new_plan(&b);
            s.put_plan(&p).await.unwrap();
            let at = crate::crew::now();
            let first = Fact {
                id: Uuid::new_v4(),
                plan: p.id,
                kind: FactKind::Fact,
                part: None,
                at,
                text: "the gateway is upgraded".into(),
                evidence: Vec::new(),
                checked: false,
                made_by: Party::manager("sarah", "Sarah"),
                replaces: None,
            };
            let second = Fact {
                id: Uuid::new_v4(),
                at: at + Duration::minutes(1),
                text: "the gateway is upgraded to 2.4".into(),
                replaces: Some(first.id),
                ..first.clone()
            };
            let checkpoint = Fact {
                id: Uuid::new_v4(),
                kind: FactKind::Checkpoint,
                part: Some(0),
                at: at + Duration::minutes(2),
                checked: true,
                replaces: None,
                ..first.clone()
            };
            for f in [&first, &second, &checkpoint] {
                s.keep_fact(f).await.unwrap();
            }
            assert_eq!(s.facts(&[p.id]).await.unwrap(), vec![checkpoint, second]);
            assert!(s.facts(&[Uuid::new_v4()]).await.unwrap().is_empty());
        })
        .await;
    }
}
