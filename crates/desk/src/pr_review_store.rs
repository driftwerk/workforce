//! Pull request reviews in Postgres: which pull request each review
//! session was started to review, and by which boss
//! (`pr_review_sessions`), and what each said of it, once per boss and
//! head (`pr_reviews`).

use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::plan::{PrRef, PrReview};
use crate::store::DeskStore;

fn review(r: &Row) -> Result<PrReview> {
    Ok(PrReview {
        boss: r.try_get(0)?,
        pr: PrRef {
            owner: r.try_get(1)?,
            repo: r.try_get(2)?,
            number: r.try_get(3)?,
        },
        head_sha: r.try_get(4)?,
        clean: r.try_get(5)?,
        by_session: r.try_get(6)?,
        at: r.try_get(7)?,
    })
}

impl DeskStore {
    /// Keeps that `boss` started `session` to review `pr`.
    pub async fn pr_review_link(&self, session: Uuid, boss: &str, pr: &PrRef) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO pr_review_sessions (session_id, boss, owner, repo, number) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (session_id) DO UPDATE \
                 SET boss = EXCLUDED.boss, owner = EXCLUDED.owner, repo = EXCLUDED.repo, \
                 number = EXCLUDED.number",
                &[&session, &boss, &pr.owner, &pr.repo, &pr.number],
            )
            .await?;
        Ok(())
    }

    /// The boss that started `session` to review a pull request, and that
    /// pull request, if it was.
    pub async fn pr_review_of(&self, session: Uuid) -> Result<Option<(String, PrRef)>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT boss, owner, repo, number FROM pr_review_sessions WHERE session_id = $1",
                &[&session],
            )
            .await?;
        row.map(|r| {
            Ok((
                r.try_get(0)?,
                PrRef {
                    owner: r.try_get(1)?,
                    repo: r.try_get(2)?,
                    number: r.try_get(3)?,
                },
            ))
        })
        .transpose()
    }

    /// Keeps `r`: at a head its boss already reviewed, its verdict replaces
    /// the one kept.
    pub async fn pr_review_put(&self, r: &PrReview) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO pr_reviews \
                 (boss, owner, repo, number, head_sha, clean, by_session, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (boss, owner, repo, number, head_sha) DO UPDATE \
                 SET clean = EXCLUDED.clean, by_session = EXCLUDED.by_session, at = EXCLUDED.at",
                &[
                    &r.boss,
                    &r.pr.owner,
                    &r.pr.repo,
                    &r.pr.number,
                    &r.head_sha,
                    &r.clean,
                    &r.by_session,
                    &r.at,
                ],
            )
            .await?;
        Ok(())
    }

    /// The newest review `boss` kept of `pr`, at whichever head.
    pub async fn pr_review_newest(&self, boss: &str, pr: &PrRef) -> Result<Option<PrReview>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "SELECT boss, owner, repo, number, head_sha, clean, by_session, at \
                 FROM pr_reviews \
                 WHERE boss = $1 AND owner = $2 AND repo = $3 AND number = $4 \
                 ORDER BY at DESC LIMIT 1",
                &[&boss, &pr.owner, &pr.repo, &pr.number],
            )
            .await?
            .as_ref()
            .map(review)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, DurationRound, Utc};
    use workforce_postgres::test_support::with_test_pool;

    use super::*;

    const MIGRATION: &str = include_str!("../migrations/V094__act_fully_ships.sql");

    fn pr(number: i64) -> PrRef {
        PrRef {
            owner: "acme".into(),
            repo: "web".into(),
            number,
        }
    }

    fn at_head(head: char, clean: bool, ago: i64) -> PrReview {
        PrReview {
            boss: "sarah".into(),
            pr: pr(42),
            head_sha: head.to_string().repeat(40),
            clean,
            by_session: Uuid::new_v4(),
            at: (Utc::now() - Duration::minutes(ago))
                .duration_trunc(Duration::microseconds(1))
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn the_migration_applies_twice_and_keeps_one_review_per_boss_and_head() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            DeskStore::with_pool(pool.clone()).await.unwrap();
            let client = pool.get().await.unwrap();
            client.batch_execute(MIGRATION).await.unwrap();
            let insert = |boss: &str| {
                format!(
                    "INSERT INTO pr_reviews \
                     (boss, owner, repo, number, head_sha, clean, by_session, at) \
                     VALUES ('{boss}', 'acme', 'web', 7, 'abc', true, \
                     '00000000-0000-0000-0000-000000000001', now())"
                )
            };
            client.batch_execute(&insert("sarah")).await.unwrap();
            assert!(
                client.batch_execute(&insert("sarah")).await.is_err(),
                "a second row of one boss at the same head is refused"
            );
            client.batch_execute(&insert("mia")).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_is_linked_to_the_pull_request_it_reviews() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let session = Uuid::new_v4();
            assert_eq!(store.pr_review_of(session).await.unwrap(), None);
            store
                .pr_review_link(session, "sarah", &pr(42))
                .await
                .unwrap();
            assert_eq!(
                store.pr_review_of(session).await.unwrap(),
                Some(("sarah".into(), pr(42)))
            );
            store
                .pr_review_link(session, "sarah", &pr(43))
                .await
                .unwrap();
            assert_eq!(
                store.pr_review_of(session).await.unwrap(),
                Some(("sarah".into(), pr(43)))
            );
        })
        .await;
    }

    #[tokio::test]
    async fn the_newest_review_of_its_own_boss_is_read_and_a_head_reviewed_again_is_updated() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            assert_eq!(
                store.pr_review_newest("sarah", &pr(42)).await.unwrap(),
                None
            );
            let older = at_head('a', false, 10);
            let newer = at_head('b', false, 5);
            store.pr_review_put(&older).await.unwrap();
            store.pr_review_put(&newer).await.unwrap();
            assert_eq!(
                store.pr_review_newest("sarah", &pr(42)).await.unwrap(),
                Some(newer)
            );
            let again = at_head('b', true, 1);
            store.pr_review_put(&again).await.unwrap();
            assert_eq!(
                store.pr_review_newest("sarah", &pr(42)).await.unwrap(),
                Some(again),
                "the same head again: its verdict is the new one"
            );
            assert_eq!(store.pr_review_newest("sarah", &pr(7)).await.unwrap(), None);
            assert_eq!(
                store.pr_review_newest("mia", &pr(42)).await.unwrap(),
                None,
                "another boss's review is never read as its own"
            );
        })
        .await;
    }
}
