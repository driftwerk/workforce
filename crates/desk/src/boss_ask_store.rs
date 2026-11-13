//! What sessions asked the person up a boss link, in Postgres
//! ([`crate::boss_ask`]).

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::boss_ask::{Ask, Choice, Class, Decider};
use crate::store::DeskStore;

/// An ask to keep, before it goes up.
#[derive(Debug, Clone)]
pub struct NewAsk<'a> {
    pub link_id: Uuid,
    pub group: &'a str,
    pub session: Uuid,
    pub approval: Uuid,
    pub nonce_hash: &'a str,
    pub class: Class,
    pub options: &'a [Choice],
    pub expires_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, link_id, group_id, session, approval, nonce_hash, class, options, \
                       state, expires_at";

fn ask_of(r: &Row) -> Result<Ask> {
    let class: String = r.try_get(6)?;
    let options: serde_json::Value = r.try_get(7)?;
    Ok(Ask {
        id: r.try_get(0)?,
        link_id: r.try_get(1)?,
        group: r.try_get(2)?,
        session: r.try_get(3)?,
        approval: r.try_get(4)?,
        nonce_hash: r.try_get(5)?,
        // What this engine can't read is answered in Desk.
        class: Class::parse(&class).unwrap_or(Class::DeskOnly),
        options: serde_json::from_value(options).unwrap_or_default(),
        state: r.try_get(8)?,
        expires_at: r.try_get(9)?,
    })
}

impl DeskStore {
    /// Keeps `ask` open, unless one for its request on its link is kept
    /// already: the new one's id when it was made.
    pub async fn boss_ask_open(&self, ask: &NewAsk<'_>) -> Result<Option<Uuid>> {
        let client = self.pool().get().await?;
        let id = Uuid::new_v4();
        let made = client
            .query_opt(
                "INSERT INTO boss_asks (id, link_id, group_id, session, approval, nonce_hash, \
                   class, options, created_at, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), $9) \
                 ON CONFLICT (link_id, approval) DO NOTHING RETURNING id",
                &[
                    &id,
                    &ask.link_id,
                    &ask.group,
                    &ask.session,
                    &ask.approval,
                    &ask.nonce_hash,
                    &ask.class.as_str(),
                    &serde_json::to_value(ask.options)?,
                    &ask.expires_at,
                ],
            )
            .await
            .context("keeping an ask")?;
        Ok(made.map(|r| r.get(0)))
    }

    /// The ask `id`, whatever its state.
    pub async fn boss_ask(&self, id: Uuid) -> Result<Option<Ask>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM boss_asks WHERE id = $1"),
                &[&id],
            )
            .await?;
        row.as_ref().map(ask_of).transpose()
    }

    /// The asks open on `link`.
    pub async fn boss_asks_open(&self, link: Uuid) -> Result<Vec<Ask>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_asks WHERE link_id = $1 AND state = 'open' \
                     ORDER BY created_at, id"
                ),
                &[&link],
            )
            .await?
            .iter()
            .map(ask_of)
            .collect()
    }

    /// A fresh nonce for the open ask `id`, as it goes up again on a new
    /// socket: the one before is good no longer. False when it isn't open.
    pub async fn boss_ask_renonce(&self, id: Uuid, nonce_hash: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE boss_asks SET nonce_hash = $2 WHERE id = $1 AND state = 'open'",
                &[&id, &nonce_hash],
            )
            .await?
            == 1)
    }

    /// Takes the open ask `id`, its nonce being `nonce_hash`, as answered
    /// with `choice` by `decider`: once. False when it was taken, closed or
    /// given a new nonce first.
    pub async fn boss_ask_take(
        &self,
        id: Uuid,
        nonce_hash: &str,
        choice: Choice,
        decider: &Decider,
    ) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE boss_asks SET state = 'decided', choice = $3, decider = $4, \
                   closed_at = now() \
                 WHERE id = $1 AND state = 'open' AND nonce_hash = $2 AND expires_at > now()",
                &[
                    &id,
                    &nonce_hash,
                    &choice.as_str(),
                    &serde_json::to_value(decider)?,
                ],
            )
            .await
            .context("taking an ask's answer")?
            == 1)
    }

    /// Closes the open ask `id` as `how`; false when it wasn't open.
    pub async fn boss_ask_close(&self, id: Uuid, how: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE boss_asks SET state = $2, closed_at = now() \
                 WHERE id = $1 AND state = 'open'",
                &[&id, &how],
            )
            .await?
            == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boss_ask::{nonce_hash, options};
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test]
    async fn an_ask_is_kept_once_per_request_and_taken_once_with_its_nonce() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let (link, session, approval) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            let first = nonce_hash("first");
            let opts = options(Class::Anywhere);
            let new = NewAsk {
                link_id: link,
                group: "acme",
                session,
                approval,
                nonce_hash: &first,
                class: Class::Anywhere,
                options: &opts,
                expires_at: Utc::now() + chrono::Duration::minutes(30),
            };
            let id = store.boss_ask_open(&new).await.unwrap().expect("kept");
            assert!(
                store.boss_ask_open(&new).await.unwrap().is_none(),
                "once a request"
            );
            let ask = store.boss_ask(id).await.unwrap().unwrap();
            assert_eq!(
                (ask.state.as_str(), ask.class, ask.options.clone()),
                ("open", Class::Anywhere, opts)
            );
            assert_eq!(store.boss_asks_open(link).await.unwrap().len(), 1);

            // A new nonce: the old one is good no longer.
            let second = nonce_hash("second");
            assert!(store.boss_ask_renonce(id, &second).await.unwrap());
            let sam = Decider {
                identity_id: "sam".into(),
                via: "slack".into(),
                slack_user_id: None,
            };
            assert!(!store
                .boss_ask_take(id, &first, Choice::Allow, &sam)
                .await
                .unwrap());
            assert!(store
                .boss_ask_take(id, &second, Choice::Allow, &sam)
                .await
                .unwrap());
            assert!(
                !store
                    .boss_ask_take(id, &second, Choice::Allow, &sam)
                    .await
                    .unwrap(),
                "good once"
            );
            assert!(!store.boss_ask_close(id, "expired").await.unwrap());
            assert_eq!(store.boss_ask(id).await.unwrap().unwrap().state, "decided");
            assert!(store.boss_asks_open(link).await.unwrap().is_empty());
        })
        .await;
    }
}
