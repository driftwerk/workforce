//! Act fully's directives in Postgres: the answers are the person's
//! preferences (`boss_preferences`, marked by `directive`), and the asking
//! is `boss_directive_asks`, one row per person and boss scope.

use eyre::Result;
use uuid::Uuid;

use crate::directive::{self, Asking, View};
use crate::preference::{PrefState, Preference, MAIN};
use crate::store::DeskStore;

impl DeskStore {
    /// The asking kept for `owner` and `boss_scope` (none yet: the defaults).
    pub async fn directive_asking(&self, owner: &str, boss_scope: &str) -> Result<Asking> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT full_since, card_message, card_at, nudged_at, skipped_at, stop_asking \
                 FROM boss_directive_asks WHERE owner = $1 AND boss_scope = $2",
                &[&owner, &boss_scope],
            )
            .await?;
        Ok(match row {
            None => Asking::default(),
            Some(r) => Asking {
                full_since: r.try_get(0)?,
                card_message: r.try_get(1)?,
                card_at: r.try_get(2)?,
                nudged_at: r.try_get(3)?,
                skipped_at: r.try_get(4)?,
                stop_asking: r.try_get(5)?,
            },
        })
    }

    async fn directive_asking_set(&self, owner: &str, boss_scope: &str, set: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO boss_directive_asks (owner, boss_scope) VALUES ($1, $2) \
                 ON CONFLICT (owner, boss_scope) DO NOTHING",
                &[&owner, &boss_scope],
            )
            .await?;
        client
            .execute(
                &format!(
                    "UPDATE boss_directive_asks SET {set} WHERE owner = $1 AND boss_scope = $2"
                ),
                &[&owner, &boss_scope],
            )
            .await?;
        Ok(())
    }

    /// The boss is at Act fully: kept from now, unless it already was.
    pub async fn directive_full_seen(&self, owner: &str, boss_scope: &str) -> Result<()> {
        self.directive_asking_set(
            owner,
            boss_scope,
            "full_since = COALESCE(boss_directive_asks.full_since, now())",
        )
        .await
    }

    /// Claims asking the card for the boss of `boss_scope`, newly at Act
    /// fully: true for the one look (of this hub's, or another instance's)
    /// that marks it at Act fully first, which alone posts the card.
    pub async fn directive_card_claim(&self, owner: &str, boss_scope: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "INSERT INTO boss_directive_asks (owner, boss_scope, full_since) VALUES ($1, $2, now()) \
                 ON CONFLICT (owner, boss_scope) DO UPDATE SET full_since = now() \
                 WHERE boss_directive_asks.full_since IS NULL",
                &[&owner, &boss_scope],
            )
            .await?;
        Ok(n == 1)
    }

    /// The bosses of `boss_scopes` were seen set to a level below Act
    /// fully: no longer at it, so Act fully turned on again asks again. A
    /// boss not seen at all (its config unreadable a moment, its group not
    /// read yet) is left as it was.
    pub async fn directive_full_off(&self, owner: &str, boss_scopes: &[String]) -> Result<()> {
        if boss_scopes.is_empty() {
            return Ok(());
        }
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_directive_asks SET full_since = NULL \
                 WHERE owner = $1 AND full_since IS NOT NULL AND boss_scope = ANY($2)",
                &[&owner, &boss_scopes],
            )
            .await?;
        Ok(())
    }

    /// The card went out as `message`: kept with when, and Skip undone.
    pub async fn directive_card_posted(
        &self,
        owner: &str,
        boss_scope: &str,
        message: &str,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO boss_directive_asks (owner, boss_scope, full_since, card_message, card_at) \
                 VALUES ($1, $2, now(), $3, now()) \
                 ON CONFLICT (owner, boss_scope) DO UPDATE SET \
                   full_since = COALESCE(boss_directive_asks.full_since, now()), \
                   card_message = $3, card_at = now(), skipped_at = NULL",
                &[&owner, &boss_scope, &message],
            )
            .await?;
        Ok(())
    }

    /// An update said what's still open, now.
    pub async fn directive_nudged(&self, owner: &str, boss_scope: &str) -> Result<()> {
        self.directive_asking_set(owner, boss_scope, "nudged_at = now()")
            .await
    }

    /// "Skip for now" (`skipped`), or undone; "Stop asking" (`stop`), or
    /// asking again.
    pub async fn directive_asking_change(
        &self,
        owner: &str,
        boss_scope: &str,
        skipped: Option<bool>,
        stop: Option<bool>,
    ) -> Result<()> {
        if let Some(s) = skipped {
            let set = match s {
                true => "skipped_at = now()",
                false => "skipped_at = NULL",
            };
            self.directive_asking_set(owner, boss_scope, set).await?;
        }
        if let Some(s) = stop {
            let set = match s {
                true => "stop_asking = true",
                // Asking again starts the day's wait afresh.
                false => "stop_asking = false, nudged_at = now()",
            };
            self.directive_asking_set(owner, boss_scope, set).await?;
        }
        Ok(())
    }

    /// `owner`'s active directive preferences for the boss of `boss_scope`.
    pub async fn directive_preferences(
        &self,
        owner: &str,
        boss_scope: &str,
    ) -> Result<Vec<Preference>> {
        Ok(self
            .preferences_of(owner, boss_scope, false)
            .await?
            .into_iter()
            .filter(|p| p.directive.is_some())
            .collect())
    }

    /// How the directives of the boss of `boss_scope` stand for `owner`,
    /// and every preference the view names (its own and the main boss's).
    pub async fn directive_view(
        &self,
        owner: &str,
        boss_scope: &str,
    ) -> Result<(View, Vec<Preference>)> {
        let here = self.directive_preferences(owner, boss_scope).await?;
        let main = match boss_scope == MAIN {
            true => Vec::new(),
            false => self.directive_preferences(owner, MAIN).await?,
        };
        let asking = self.directive_asking(owner, boss_scope).await?;
        let v = directive::view(boss_scope, &here, &main, &asking);
        let mut all = here;
        all.extend(main);
        Ok((v, all))
    }

    /// Answers directive `key` for the boss of `boss_scope` with `answer`
    /// (its key or label), retiring the answer it had; `None` clears it.
    /// Err with the person's words when `key` or `answer` isn't one.
    pub async fn directive_answer(
        &self,
        owner: &str,
        boss_scope: &str,
        key: &str,
        answer: Option<&str>,
    ) -> Result<std::result::Result<Option<Preference>, String>> {
        let Some(d) = directive::find(key) else {
            return Ok(Err(format!("“{key}” isn't one of the directives.")));
        };
        let answer = answer.map(str::trim).filter(|a| !a.is_empty());
        let chosen = match answer {
            None => None,
            Some(a) => match d.answer(a) {
                Some(a) => Some(a),
                None => {
                    let labels: Vec<&str> = d.answers.iter().map(|a| a.label).collect();
                    return Ok(Err(format!(
                        "“{a}” isn't an answer to “{}”: {}.",
                        d.question,
                        labels.join(", ")
                    )));
                }
            },
        };
        // Set aside ones too (paused after undos): one answer stands.
        let had: Vec<Preference> = self
            .preferences_of(owner, boss_scope, true)
            .await?
            .into_iter()
            .filter(|p| p.state != PrefState::Retired && p.directive.as_deref() == Some(d.key))
            .collect();
        let Some(chosen) = chosen else {
            for p in &had {
                self.preference_set_state(p.id, PrefState::Retired).await?;
            }
            return Ok(Ok(None));
        };
        let mut p = directive::answered(boss_scope, d, chosen);
        p.owner = owner.to_string();
        // One kept answer per directive: the old one gives way.
        for o in &had {
            self.preference_set_state(o.id, PrefState::Retired).await?;
        }
        self.preference_keep(&p).await?;
        Ok(Ok(Some(p)))
    }

    /// Keeps one of the person's own directives for the boss of
    /// `boss_scope`.
    pub async fn directive_own(
        &self,
        owner: &str,
        boss_scope: &str,
        text: &str,
    ) -> Result<std::result::Result<Preference, String>> {
        let mut p = match directive::own(boss_scope, text) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        p.owner = owner.to_string();
        // Never preference_keep: their own all share one case, and each
        // stands.
        self.preference_put(&p).await?;
        Ok(Ok(p))
    }

    /// The directive preference `id` of `owner`'s, if it is one.
    pub async fn directive_of(&self, owner: &str, id: Uuid) -> Result<Option<Preference>> {
        Ok(self
            .preference(id)
            .await?
            .filter(|p| p.owner == owner && p.directive.is_some() && p.state != PrefState::Retired))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::YOU;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test]
    async fn directives_are_answered_changed_cleared_and_asked_once_per_act_fully() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            // Answered for the main boss: a group's boss sees it.
            let kept = store
                .directive_answer(YOU, MAIN, "merge", Some("After checks pass"))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(kept.directive_answer.as_deref(), Some("checks"));
            let (v, _) = store.directive_view(YOU, "g1").await.unwrap();
            assert_eq!(v.questions[0].answer.as_deref(), Some("checks"));
            assert!(v.questions[0].inherited);
            // Answered again: one answer, the new one.
            store
                .directive_answer(YOU, MAIN, "merge", Some("no"))
                .await
                .unwrap()
                .unwrap();
            let mine = store.directive_preferences(YOU, MAIN).await.unwrap();
            assert_eq!(mine.len(), 1);
            assert_eq!(mine[0].directive_answer.as_deref(), Some("no"));
            // Bad keys and answers are said, not kept.
            assert!(store
                .directive_answer(YOU, MAIN, "nope", Some("yes"))
                .await
                .unwrap()
                .is_err());
            assert!(store
                .directive_answer(YOU, MAIN, "merge", Some("maybe"))
                .await
                .unwrap()
                .is_err());
            // Their own each stand.
            store
                .directive_own(YOU, MAIN, "Never touch billing")
                .await
                .unwrap()
                .unwrap();
            store
                .directive_own(YOU, MAIN, "Keep PRs small")
                .await
                .unwrap()
                .unwrap();
            let (v, _) = store.directive_view(YOU, MAIN).await.unwrap();
            assert_eq!(v.own.len(), 2);
            // Cleared.
            store
                .directive_answer(YOU, MAIN, "merge", None)
                .await
                .unwrap()
                .unwrap();
            let (v, _) = store.directive_view(YOU, MAIN).await.unwrap();
            assert!(v.questions[0].answer.is_none());

            // The asking.
            let a = store.directive_asking(YOU, MAIN).await.unwrap();
            assert_eq!(a, Asking::default());
            store.directive_card_posted(YOU, MAIN, "m1").await.unwrap();
            let a = store.directive_asking(YOU, MAIN).await.unwrap();
            assert!(a.full_since.is_some() && a.card_at.is_some());
            assert_eq!(a.card_message.as_deref(), Some("m1"));
            // Off Act fully: asked again next time.
            // Not seen at all: as it was.
            store.directive_full_off(YOU, &[]).await.unwrap();
            assert!(store
                .directive_asking(YOU, MAIN)
                .await
                .unwrap()
                .full_since
                .is_some());
            store
                .directive_full_off(YOU, &[MAIN.to_string()])
                .await
                .unwrap();
            assert!(store
                .directive_asking(YOU, MAIN)
                .await
                .unwrap()
                .full_since
                .is_none());
            store
                .directive_asking_change(YOU, MAIN, Some(true), Some(true))
                .await
                .unwrap();
            let a = store.directive_asking(YOU, MAIN).await.unwrap();
            assert!(a.stop_asking && a.skipped_at.is_some());
            store
                .directive_asking_change(YOU, "g2", None, Some(true))
                .await
                .unwrap();
            assert!(store.directive_asking(YOU, "g2").await.unwrap().stop_asking);
            store.directive_nudged(YOU, MAIN).await.unwrap();
            assert!(store
                .directive_asking(YOU, MAIN)
                .await
                .unwrap()
                .nudged_at
                .is_some());
        })
        .await;
    }
}
