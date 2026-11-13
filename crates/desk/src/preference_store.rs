//! The person's preferences in Postgres (`boss_preferences`): kept, listed
//! per person and boss scope, changed, set aside and forgotten by id, and
//! counted as they are applied and overruled.

use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::preference::{PrefScope, PrefSource, PrefState, Preference};
use crate::store::DeskStore;

const COLUMNS: &str = "id, owner, boss_scope, when_text, then_text, scope, source, from_item, \
     created_at, changed_at, applied_count, last_applied_at, overruled_count, state";

/// After this many overrules a preference is set aside until the person
/// says otherwise.
pub const PAUSE_AFTER_OVERRULES: i32 = 2;

fn preference(r: &Row) -> Result<Preference> {
    let scope: String = r.try_get(5)?;
    Ok(Preference {
        id: r.try_get(0)?,
        owner: r.try_get(1)?,
        boss_scope: r.try_get(2)?,
        when_text: r.try_get(3)?,
        then_text: r.try_get(4)?,
        scope: PrefScope::parse(&scope).map_err(|e| eyre::eyre!(e))?,
        source: PrefSource::parse(r.try_get(6)?)?,
        from_item: r.try_get(7)?,
        created_at: r.try_get(8)?,
        changed_at: r.try_get(9)?,
        applied_count: r.try_get(10)?,
        last_applied_at: r.try_get(11)?,
        overruled_count: r.try_get(12)?,
        state: PrefState::parse(r.try_get(13)?)?,
    })
}

/// A change to a preference's words or scope; what is `None` stays.
#[derive(Debug, Clone, Default)]
pub struct PrefChange {
    pub when_text: Option<String>,
    pub then_text: Option<String>,
    pub scope: Option<PrefScope>,
}

impl DeskStore {
    /// Keeps `p`, new.
    pub async fn preference_put(&self, p: &Preference) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO boss_preferences ({COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
                ),
                &[
                    &p.id,
                    &p.owner,
                    &p.boss_scope,
                    &p.when_text,
                    &p.then_text,
                    &p.scope.text(),
                    &p.source.as_str(),
                    &p.from_item,
                    &p.created_at,
                    &p.changed_at,
                    &p.applied_count,
                    &p.last_applied_at,
                    &p.overruled_count,
                    &p.state.as_str(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn preference(&self, id: Uuid) -> Result<Option<Preference>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM boss_preferences WHERE id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(preference)
            .transpose()
    }

    /// The preferences `ids`, by id.
    pub async fn preferences_by_id(&self, ids: &[Uuid]) -> Result<Vec<Preference>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {COLUMNS} FROM boss_preferences WHERE id = ANY($1)"),
                &[&ids],
            )
            .await?
            .iter()
            .map(preference)
            .collect()
    }

    /// The preference kept from open item `item` ("always"), if one is
    /// and it isn't forgotten.
    pub async fn preference_from_item(&self, item: Uuid) -> Result<Option<Preference>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM boss_preferences \
                     WHERE from_item = $1 AND state <> 'retired' \
                     ORDER BY created_at DESC LIMIT 1"
                ),
                &[&item],
            )
            .await?
            .as_ref()
            .map(preference)
            .transpose()
    }

    /// `owner`'s preferences for the boss of `boss_scope`: the active
    /// ones, or (`all`) the paused and retired too. Oldest first.
    pub async fn preferences_of(
        &self,
        owner: &str,
        boss_scope: &str,
        all: bool,
    ) -> Result<Vec<Preference>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_preferences \
                     WHERE owner = $1 AND boss_scope = $2 AND ($3 OR state = 'active') \
                     ORDER BY created_at, id"
                ),
                &[&owner, &boss_scope, &all],
            )
            .await?
            .iter()
            .map(preference)
            .collect()
    }

    /// Changes `id`'s words or scope: the preference as it now is, or
    /// `None` when there is none (or it is retired).
    pub async fn preference_change(
        &self,
        id: Uuid,
        change: &PrefChange,
    ) -> Result<Option<Preference>> {
        let client = self.pool().get().await?;
        let scope = change.scope.as_ref().map(PrefScope::text);
        client
            .query_opt(
                &format!(
                    "UPDATE boss_preferences SET \
                       when_text = COALESCE($2, when_text), \
                       then_text = COALESCE($3, then_text), \
                       scope = COALESCE($4, scope), \
                       changed_at = now() \
                     WHERE id = $1 AND state <> 'retired' RETURNING {COLUMNS}"
                ),
                &[&id, &change.when_text, &change.then_text, &scope],
            )
            .await?
            .as_ref()
            .map(preference)
            .transpose()
    }

    /// Sets `id` to `state`; resuming one also starts its overrules
    /// again. The preference as it now is, or `None` when there is none.
    pub async fn preference_set_state(
        &self,
        id: Uuid,
        state: PrefState,
    ) -> Result<Option<Preference>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_preferences SET state = $2, changed_at = now(), \
                       overruled_count = CASE WHEN $2 = 'active' THEN 0 ELSE overruled_count END \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ),
                &[&id, &state.as_str()],
            )
            .await?
            .as_ref()
            .map(preference)
            .transpose()
    }

    /// `id` made a decision: counted, with when.
    pub async fn preference_applied(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_preferences SET applied_count = applied_count + 1, \
                 last_applied_at = now() WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(())
    }

    /// The person undid a decision `id` made: counted, and an active one
    /// set aside at [`PAUSE_AFTER_OVERRULES`]. The preference as it now is.
    pub async fn preference_overruled(&self, id: Uuid) -> Result<Option<Preference>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_preferences SET overruled_count = overruled_count + 1, \
                       changed_at = now(), \
                       state = CASE WHEN state = 'active' AND overruled_count + 1 >= $2 \
                                    THEN 'paused' ELSE state END \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ),
                &[&id, &PAUSE_AFTER_OVERRULES],
            )
            .await?
            .as_ref()
            .map(preference)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preference::MAIN;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test]
    async fn a_preference_is_kept_listed_changed_counted_and_forgotten() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let p = Preference::new(
                MAIN,
                "When a session wants to merge with CI red",
                "make it wait for green.",
                PrefScope::Repo("acme/web".into()),
                PrefSource::Said,
            );
            store.preference_put(&p).await.unwrap();
            let other = Preference::new("g-1", "x", "y", PrefScope::All, PrefSource::Said);
            store.preference_put(&other).await.unwrap();
            let mine = store.preferences_of("you", MAIN, false).await.unwrap();
            assert_eq!(mine.len(), 1, "only the main boss's");
            assert_eq!(mine[0], p);
            assert_eq!(mine[0].when_text, "a session wants to merge with CI red");
            assert_eq!(mine[0].then_text, "make it wait for green");

            let changed = store
                .preference_change(
                    p.id,
                    &PrefChange {
                        then_text: Some("wait for green, then merge".into()),
                        scope: Some(PrefScope::All),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(changed.then_text, "wait for green, then merge");
            assert_eq!(changed.scope, PrefScope::All);
            assert_eq!(changed.when_text, p.when_text, "what isn't said stays");

            store.preference_applied(p.id).await.unwrap();
            store.preference_applied(p.id).await.unwrap();
            let used = store.preference(p.id).await.unwrap().unwrap();
            assert_eq!(used.applied_count, 2);
            assert!(used.last_applied_at.is_some());

            // Two overrules set it aside; resuming starts the count again.
            let once = store.preference_overruled(p.id).await.unwrap().unwrap();
            assert_eq!((once.overruled_count, once.state), (1, PrefState::Active));
            let twice = store.preference_overruled(p.id).await.unwrap().unwrap();
            assert_eq!((twice.overruled_count, twice.state), (2, PrefState::Paused));
            assert!(store
                .preferences_of("you", MAIN, false)
                .await
                .unwrap()
                .is_empty());
            let back = store
                .preference_set_state(p.id, PrefState::Active)
                .await
                .unwrap()
                .unwrap();
            assert_eq!((back.overruled_count, back.state), (0, PrefState::Active));

            let gone = store
                .preference_set_state(p.id, PrefState::Retired)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(gone.state, PrefState::Retired);
            assert!(store
                .preference_change(p.id, &PrefChange::default())
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                store.preferences_of("you", MAIN, true).await.unwrap().len(),
                1,
                "kept, so a decision that cites it still reads true"
            );
        })
        .await;
    }
}
