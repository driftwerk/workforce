//! A boss's holds on its sessions at Act fully, each on one of the
//! person's directives (`boss_holds`): kept in Postgres, so a hold lasts
//! until what ends it, whatever restarts in between.

use eyre::{eyre, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::store::DeskStore;

/// What ends a hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    PersonAnswers,
    DirectiveWithdrawn,
    /// What the boss records as met, in words ("CI green at the head").
    Condition(String),
}

impl Until {
    /// What a boss said ends it: `person_answers`, `directive_withdrawn`,
    /// or a condition in its own words. None for nothing said.
    pub fn of(raw: &str) -> Option<Self> {
        match raw.trim() {
            "" => None,
            "person_answers" => Some(Self::PersonAnswers),
            "directive_withdrawn" => Some(Self::DirectiveWithdrawn),
            c => Some(Self::Condition(c.to_string())),
        }
    }

    fn columns(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::PersonAnswers => ("person_answers", None),
            Self::DirectiveWithdrawn => ("directive_withdrawn", None),
            Self::Condition(c) => ("condition", Some(c)),
        }
    }

    fn read(until: &str, condition: Option<String>) -> Result<Self> {
        match (until, condition) {
            ("person_answers", _) => Ok(Self::PersonAnswers),
            ("directive_withdrawn", _) => Ok(Self::DirectiveWithdrawn),
            ("condition", Some(c)) => Ok(Self::Condition(c)),
            (other, _) => Err(eyre!("a hold kept as {other:?} isn't one")),
        }
    }
}

/// A session held on one of the person's directives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub session: Uuid,
    pub boss: String,
    /// The person whose directive it is.
    pub owner: String,
    pub directive_id: Uuid,
    /// The open item that asks them its question.
    pub question: Uuid,
    pub until: Until,
}

fn hold(r: &Row) -> Result<Hold> {
    Ok(Hold {
        session: r.try_get(0)?,
        boss: r.try_get(1)?,
        owner: r.try_get(2)?,
        directive_id: r.try_get(3)?,
        question: r.try_get(4)?,
        until: Until::read(r.try_get(5)?, r.try_get(6)?)?,
    })
}

impl DeskStore {
    /// Keeps `h`, in place of any hold its session was under.
    pub async fn hold_put(&self, h: &Hold) -> Result<()> {
        let (until, condition) = h.until.columns();
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO boss_holds \
                 (session_id, boss, owner, directive_id, question, until, condition) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (session_id) DO UPDATE \
                 SET boss = EXCLUDED.boss, owner = EXCLUDED.owner, \
                 directive_id = EXCLUDED.directive_id, question = EXCLUDED.question, \
                 until = EXCLUDED.until, condition = EXCLUDED.condition",
                &[
                    &h.session,
                    &h.boss,
                    &h.owner,
                    &h.directive_id,
                    &h.question,
                    &until,
                    &condition,
                ],
            )
            .await?;
        Ok(())
    }

    /// The hold `session` is under, if any.
    pub async fn hold_of(&self, session: Uuid) -> Result<Option<Hold>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "SELECT session_id, boss, owner, directive_id, question, until, condition \
                 FROM boss_holds WHERE session_id = $1",
                &[&session],
            )
            .await?
            .as_ref()
            .map(hold)
            .transpose()
    }

    /// Ends `session`'s hold: the one it ended, if it was held.
    pub async fn hold_end(&self, session: Uuid) -> Result<Option<Hold>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "DELETE FROM boss_holds WHERE session_id = $1 \
                 RETURNING session_id, boss, owner, directive_id, question, until, condition",
                &[&session],
            )
            .await?
            .as_ref()
            .map(hold)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;

    fn held(session: Uuid, until: Until) -> Hold {
        Hold {
            session,
            boss: "sarah".into(),
            owner: "you".into(),
            directive_id: Uuid::new_v4(),
            question: Uuid::new_v4(),
            until,
        }
    }

    #[tokio::test]
    async fn a_hold_is_kept_replaced_and_ended_with_what_ends_it() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let s = Uuid::new_v4();
            assert_eq!(store.hold_of(s).await.unwrap(), None);
            for until in [
                Until::PersonAnswers,
                Until::DirectiveWithdrawn,
                Until::Condition("CI green at the head".into()),
            ] {
                let h = held(s, until);
                store.hold_put(&h).await.unwrap();
                assert_eq!(store.hold_of(s).await.unwrap(), Some(h));
            }
            let last = store.hold_of(s).await.unwrap();
            assert_eq!(store.hold_end(s).await.unwrap(), last);
            assert_eq!(store.hold_of(s).await.unwrap(), None);
            assert_eq!(store.hold_end(s).await.unwrap(), None);
        })
        .await;
    }

    #[test]
    fn what_ends_a_hold_is_read_as_said() {
        assert_eq!(Until::of(" "), None);
        assert_eq!(Until::of("person_answers"), Some(Until::PersonAnswers));
        assert_eq!(
            Until::of("directive_withdrawn"),
            Some(Until::DirectiveWithdrawn)
        );
        assert_eq!(
            Until::of(" CI green "),
            Some(Until::Condition("CI green".into()))
        );
        assert!(Until::read("later", None).is_err());
        assert!(Until::read("condition", None).is_err());
    }
}
