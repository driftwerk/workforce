//! Budgets and breakers in Postgres, and the ledger summed as budgets count
//! it.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use uuid::Uuid;
use workforce_core::budget::{Budget, BudgetScope, OnExceed, Period, Unit};

use crate::breaker;
use crate::store::DeskStore;

/// What a set of sessions spent from a moment on: every token, and the
/// dollars of the calls that have a price.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Used {
    pub tokens: u64,
    pub usd: f64,
}

/// What one session spent: in all, and in each group, by the group each
/// ledger row was stamped with when written. A group's budgets count what
/// was spent in the group, so a session moved in brings none of its past.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSpend {
    pub all: Used,
    pub groups: HashMap<String, Used>,
}

impl SessionSpend {
    /// What counts toward `b`: a group's budget its group's part, any
    /// other the whole.
    pub fn toward(&self, b: &Budget) -> Used {
        match &b.group {
            Some(g) => self.groups.get(g).copied().unwrap_or_default(),
            None => self.all,
        }
    }
}

impl Used {
    pub fn add(&mut self, other: Used) {
        self.tokens += other.tokens;
        self.usd += other.usd;
    }

    /// In `unit`.
    pub fn of(&self, unit: Unit) -> f64 {
        match unit {
            Unit::Tokens => self.tokens as f64,
            Unit::Usd => self.usd,
        }
    }
}

fn budget_of(r: &tokio_postgres::Row) -> Result<Budget> {
    Ok(Budget {
        id: r.try_get("id")?,
        scope: BudgetScope::parse(r.try_get("scope")?)?,
        group: r.try_get("group_id")?,
        period: Period::parse(r.try_get("period")?)?,
        unit: Unit::parse(r.try_get("unit")?)?,
        amount: r.try_get("amount")?,
        warn_at: r.try_get("warn_at")?,
        on_exceed: OnExceed::parse(r.try_get("on_exceed")?)?,
        updated_at: r.try_get("updated_at")?,
    })
}

impl DeskStore {
    /// Every budget the person set.
    pub async fn budgets(&self) -> Result<Vec<Budget>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT id, scope, group_id, period, unit, amount, warn_at, on_exceed, updated_at \
                 FROM budgets ORDER BY updated_at",
                &[],
            )
            .await
            .context("reading budgets")?;
        // One a newer hub kept that this one can't read is left out.
        Ok(rows.iter().filter_map(|r| budget_of(r).ok()).collect())
    }

    /// Keeps `b`, or replaces the one with its group, scope, period and
    /// unit; what it is kept as (that one's id when it replaced one).
    pub async fn put_budget(&self, b: &Budget) -> Result<Budget> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "INSERT INTO budgets (id, scope, group_id, period, unit, amount, warn_at, \
                   on_exceed, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (COALESCE(group_id, ''), scope, period, unit) DO UPDATE SET \
                   amount = EXCLUDED.amount, warn_at = EXCLUDED.warn_at, \
                   on_exceed = EXCLUDED.on_exceed, updated_at = EXCLUDED.updated_at \
                 RETURNING id, scope, group_id, period, unit, amount, warn_at, on_exceed, \
                   updated_at",
                &[
                    &b.id,
                    &b.scope.text(),
                    &b.group,
                    &b.period.as_str(),
                    &b.unit.as_str(),
                    &b.amount,
                    &b.warn_at,
                    &b.on_exceed.as_str(),
                    &b.updated_at,
                ],
            )
            .await
            .context("keeping a budget")?;
        budget_of(&row)
    }

    /// Changes the budget `id` to `b`'s amounts; `None` when there is none.
    pub async fn update_budget(&self, b: &Budget) -> Result<Option<Budget>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "UPDATE budgets SET amount = $2, warn_at = $3, on_exceed = $4, updated_at = $5 \
                 WHERE id = $1 \
                 RETURNING id, scope, group_id, period, unit, amount, warn_at, on_exceed, \
                   updated_at",
                &[
                    &b.id,
                    &b.amount,
                    &b.warn_at,
                    &b.on_exceed.as_str(),
                    &b.updated_at,
                ],
            )
            .await
            .context("changing a budget")?;
        row.as_ref().map(budget_of).transpose()
    }

    /// Removes the budget `id`; whether there was one.
    pub async fn delete_budget(&self, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute("DELETE FROM budgets WHERE id = $1", &[&id])
            .await
            .context("removing a budget")?
            > 0)
    }

    /// Every subject's breaker, as kept. One a newer hub kept that this one
    /// can't read starts clear.
    pub async fn breakers(&self) -> Result<HashMap<String, breaker::State>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT subject, state FROM breakers", &[])
            .await
            .context("reading breakers")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let subject: String = r.try_get(0).ok()?;
                let state = serde_json::from_value(r.try_get(1).ok()?).unwrap_or_default();
                Some((subject, state))
            })
            .collect())
    }

    /// Keeps `subject`'s breaker; a clear one with nothing to remember is
    /// removed.
    pub async fn put_breaker(&self, subject: &str, state: &breaker::State) -> Result<()> {
        let client = self.pool().get().await?;
        if *state == breaker::State::default() {
            client
                .execute("DELETE FROM breakers WHERE subject = $1", &[&subject])
                .await
                .context("clearing a breaker")?;
            return Ok(());
        }
        client
            .execute(
                "INSERT INTO breakers (subject, state, updated_at) VALUES ($1, $2, now()) \
                 ON CONFLICT (subject) DO UPDATE SET state = EXCLUDED.state, \
                   updated_at = EXCLUDED.updated_at",
                &[&subject, &serde_json::to_value(state)?],
            )
            .await
            .context("keeping a breaker")?;
        Ok(())
    }

    /// What each session spent from `from` on, in all and in each group its
    /// rows were stamped with.
    pub async fn ledger_used(&self, from: DateTime<Utc>) -> Result<HashMap<Uuid, SessionSpend>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT session, group_id, \
                   SUM(input + output + cache_read + cache_write)::int8, \
                   COALESCE(SUM(usd), 0)::float8 \
                 FROM ledger WHERE at >= $1 GROUP BY session, group_id",
                &[&from],
            )
            .await
            .context("summing the ledger for budgets")?;
        let mut spent: HashMap<Uuid, SessionSpend> = HashMap::new();
        for r in &rows {
            let used = Used {
                tokens: u64::try_from(r.try_get::<_, i64>(2)?).unwrap_or(0),
                usd: r.try_get(3)?,
            };
            let s = spent.entry(r.try_get(0)?).or_default();
            s.all.add(used);
            if let Some(g) = r.try_get::<_, Option<String>>(1)? {
                s.groups.entry(g).or_default().add(used);
            }
        }
        Ok(spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use workforce_core::budget::WARN_AT;
    use workforce_postgres::test_support::with_test_pool;

    fn budget(scope: BudgetScope, group: Option<&str>, amount: f64) -> Budget {
        Budget {
            id: Uuid::new_v4(),
            scope,
            group: group.map(str::to_string),
            period: Period::Day,
            unit: Unit::Usd,
            amount,
            warn_at: WARN_AT,
            on_exceed: OnExceed::Pause,
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_budget_for_the_same_scope_period_and_unit_replaces_the_one_there() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            assert!(store.budgets().await.unwrap().is_empty());
            let first = store
                .put_budget(&budget(BudgetScope::Desk, None, 10.0))
                .await
                .unwrap();
            let again = store
                .put_budget(&budget(BudgetScope::Desk, None, 20.0))
                .await
                .unwrap();
            assert_eq!((again.id, again.amount), (first.id, 20.0));
            let acme = store
                .put_budget(&budget(BudgetScope::Desk, Some("acme"), 5.0))
                .await
                .unwrap();
            assert_ne!(acme.id, first.id, "a group's total is its own");
            assert_eq!(store.budgets().await.unwrap().len(), 2);
            let mut changed = acme.clone();
            changed.amount = 6.0;
            assert_eq!(
                store.update_budget(&changed).await.unwrap().unwrap().amount,
                6.0
            );
            assert!(store.delete_budget(acme.id).await.unwrap());
            assert!(!store.delete_budget(acme.id).await.unwrap());
            assert!(store.update_budget(&changed).await.unwrap().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn a_breaker_is_kept_until_it_has_nothing_to_remember() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut state = breaker::State::default();
            state.released.push((breaker::Reason::Loop, Utc::now()));
            store.put_breaker("session:x", &state).await.unwrap();
            assert_eq!(store.breakers().await.unwrap()["session:x"], state);
            store
                .put_breaker("session:x", &breaker::State::default())
                .await
                .unwrap();
            assert!(store.breakers().await.unwrap().is_empty());
        })
        .await;
    }
}
