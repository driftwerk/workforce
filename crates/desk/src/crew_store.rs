//! The crew's tables in the `desk` schema: activity, suggestions, the extras
//! of the overseer's messages, and its check-ins.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde_json::Value;
use tokio_postgres::Row;

use crate::conversations::Speaker;
use crate::crew::{Event, Extras, Filter, Kind, Party, Suggestion, SuggestionState};
use crate::store::DeskStore;

/// One of the overseer's unprompted turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckIn {
    pub id: String,
    pub at: DateTime<Utc>,
    pub agent_id: String,
    /// `wake`, `morning` or `evening`.
    pub kind: String,
    pub prompt: String,
    pub reply: Option<String>,
}

fn party(v: Option<Value>) -> Result<Option<Party>> {
    v.filter(|v| !v.is_null())
        .map(serde_json::from_value)
        .transpose()
        .context("reading a party")
}

fn event(row: &Row) -> Result<Event> {
    let kind: String = row.try_get("kind")?;
    Ok(Event {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        kind: Kind::parse(&kind).ok_or_else(|| eyre::eyre!("unknown activity kind {kind:?}"))?,
        actor: party(Some(row.try_get("actor")?))?
            .ok_or_else(|| eyre::eyre!("an event without an actor"))?,
        subject: party(row.try_get("subject")?)?,
        text: row.try_get("text")?,
        reason: row.try_get("reason")?,
        suggestion_id: row.try_get("suggestion_id")?,
        approval_id: row.try_get("approval_id")?,
        hop: row.try_get("hop")?,
        detail: row.try_get("detail")?,
        place: row
            .try_get::<_, Option<Value>>("place")?
            .map(serde_json::from_value)
            .transpose()
            .context("reading an event's place")?,
        message: row.try_get("message")?,
    })
}

fn suggestion(row: &Row) -> Result<Suggestion> {
    Ok(Suggestion {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        text: row.try_get("text")?,
        action: row.try_get("action")?,
        state: SuggestionState::parse(row.try_get("state")?),
        subject: party(row.try_get("subject")?)?,
        agent_id: row.try_get("agent_id")?,
    })
}

fn check_in(row: &Row) -> Result<CheckIn> {
    Ok(CheckIn {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        agent_id: row.try_get("agent_id")?,
        kind: row.try_get("kind")?,
        prompt: row.try_get("prompt")?,
        reply: row.try_get("reply")?,
    })
}

const NEEDS_YOU: &str = "kind IN ('needs_you', 'question')";
const BY_MANAGER: &str = "actor ->> 'kind' = 'manager'";

impl DeskStore {
    pub async fn add_event(&self, e: &Event) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO activity (id, at, kind, actor, subject, text, reason, suggestion_id, \
                 approval_id, hop, detail, place, message) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT (id) DO NOTHING",
                &[
                    &e.id,
                    &e.at,
                    &e.kind.as_str(),
                    &serde_json::to_value(&e.actor)?,
                    &e.subject.as_ref().map(serde_json::to_value).transpose()?,
                    &e.text,
                    &e.reason,
                    &e.suggestion_id,
                    &e.approval_id,
                    &e.hop,
                    &e.detail,
                    &e.place.as_ref().map(serde_json::to_value).transpose()?,
                    &e.message,
                ],
            )
            .await
            .context("recording activity")?;
        Ok(())
    }

    /// Up to `limit` events of `filter`, newest first, from before the
    /// event `before`; and whether more remain.
    pub async fn events(
        &self,
        filter: Filter,
        before: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<Event>, bool)> {
        let client = self.pool().get().await?;
        let mut clauses = vec![];
        match filter {
            Filter::All => {}
            Filter::NeedsYou => clauses.push(NEEDS_YOU.to_string()),
            Filter::Manager => clauses.push(BY_MANAGER.to_string()),
        }
        let cursor = match before {
            Some(id) => {
                let Some(row) = client
                    .query_opt("SELECT at FROM activity WHERE id = $1", &[&id])
                    .await?
                else {
                    return Ok((vec![], false));
                };
                Some((row.try_get::<_, DateTime<Utc>>("at")?, id.to_string()))
            }
            None => None,
        };
        if cursor.is_some() {
            clauses.push("(at, id) < ($2, $3)".into());
        }
        let wher = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!("SELECT * FROM activity {wher} ORDER BY at DESC, id DESC LIMIT $1");
        let take = limit + 1;
        let rows = match &cursor {
            Some((at, id)) => client.query(&sql, &[&take, at, id]).await?,
            None => client.query(&sql, &[&take]).await?,
        };
        let mut events = rows.iter().map(event).collect::<Result<Vec<_>>>()?;
        let more = events.len() as i64 > limit;
        events.truncate(limit as usize);
        Ok((events, more))
    }

    /// Events since `since`, oldest first, about the subjects in `ids` (all
    /// when empty), at most `limit`.
    pub async fn events_since(
        &self,
        since: DateTime<Utc>,
        ids: &[String],
        limit: i64,
    ) -> Result<Vec<Event>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM (SELECT * FROM activity WHERE at >= $1 \
                 AND (cardinality($2::text[]) = 0 OR subject ->> 'id' = ANY($2)) \
                 ORDER BY at DESC, id DESC LIMIT $3) recent ORDER BY at, id",
                &[&since, &ids, &limit],
            )
            .await?;
        rows.iter().map(event).collect()
    }

    /// The events of the subjects in `ids` that can be open needs, oldest
    /// first: the "Needs you" of each request in `approvals`, and each
    /// subject's latest question, however old.
    pub async fn need_events(&self, ids: &[String], approvals: &[String]) -> Result<Vec<Event>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM ( \
                   SELECT * FROM activity WHERE kind = 'needs_you' \
                     AND subject ->> 'id' = ANY($1) AND approval_id = ANY($2) \
                   UNION ALL \
                   SELECT * FROM (SELECT DISTINCT ON (subject ->> 'kind', subject ->> 'id') * \
                     FROM activity WHERE kind = 'question' AND subject ->> 'id' = ANY($1) \
                     ORDER BY subject ->> 'kind', subject ->> 'id', at DESC, id DESC) latest \
                 ) open ORDER BY at, id",
                &[&ids, &approvals],
            )
            .await?;
        rows.iter().map(event).collect()
    }

    /// Each agent's latest event of its own about a turn.
    pub async fn latest_turn_events(&self) -> Result<HashMap<String, Event>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT DISTINCT ON (subject ->> 'id') * FROM activity \
                 WHERE subject ->> 'kind' = 'agent' AND actor ->> 'kind' = 'agent' \
                 AND kind IN ('started', 'finished', 'question', 'failed', 'limit') \
                 ORDER BY subject ->> 'id', at DESC, id DESC",
                &[],
            )
            .await?;
        rows.iter()
            .map(|r| {
                let e = event(r)?;
                Ok((
                    e.subject.as_ref().map(|s| s.id.clone()).unwrap_or_default(),
                    e,
                ))
            })
            .collect()
    }

    pub async fn put_suggestion(&self, s: &Suggestion) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO suggestions (id, at, agent_id, text, action, state, subject, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,now()) ON CONFLICT (id) DO UPDATE SET state = $6, \
                 updated_at = now()",
                &[
                    &s.id,
                    &s.at,
                    &s.agent_id,
                    &s.text,
                    &s.action,
                    &s.state.as_str(),
                    &s.subject.as_ref().map(serde_json::to_value).transpose()?,
                ],
            )
            .await
            .context("saving a suggestion")?;
        Ok(())
    }

    pub async fn suggestion(&self, id: &str) -> Result<Option<Suggestion>> {
        let client = self.pool().get().await?;
        client
            .query_opt("SELECT * FROM suggestions WHERE id = $1", &[&id])
            .await?
            .as_ref()
            .map(suggestion)
            .transpose()
    }

    /// The latest suggestions, newest first.
    pub async fn suggestions(&self, limit: i64) -> Result<Vec<Suggestion>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM suggestions ORDER BY at DESC, id DESC LIMIT $1",
                &[&limit],
            )
            .await?;
        rows.iter().map(suggestion).collect()
    }

    /// Suggestions `agent_id` made since `since`, oldest first.
    pub async fn suggestions_since(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<Suggestion>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM suggestions WHERE agent_id = $1 AND at >= $2 ORDER BY at, id",
                &[&agent_id, &since],
            )
            .await?;
        rows.iter().map(suggestion).collect()
    }

    pub async fn put_extras(
        &self,
        message_id: &str,
        agent_id: &str,
        extras: &Extras,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO message_extras (message_id, agent_id, extras, at) VALUES ($1,$2,$3,now()) \
                 ON CONFLICT (message_id) DO UPDATE SET extras = $3",
                &[&message_id, &agent_id, &serde_json::to_value(extras)?],
            )
            .await
            .context("saving a message's extras")?;
        Ok(())
    }

    /// The extras of whichever of `ids` have some.
    pub async fn extras(&self, ids: &[String]) -> Result<HashMap<String, Extras>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT message_id, extras FROM message_extras WHERE message_id = ANY($1)",
                &[&ids],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get("message_id")?,
                    serde_json::from_value(r.try_get("extras")?)?,
                ))
            })
            .collect()
    }

    /// Keeps who said the DM message `message_id`, when its role doesn't.
    pub async fn put_author(
        &self,
        message_id: &str,
        agent_id: &str,
        author: &Speaker,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO message_authors (message_id, agent_id, author, at) VALUES ($1,$2,$3,now()) \
                 ON CONFLICT (message_id) DO UPDATE SET author = $3",
                &[&message_id, &agent_id, &serde_json::to_value(author)?],
            )
            .await
            .context("saving a message's author")?;
        Ok(())
    }

    /// The authors kept for whichever of `ids` have one.
    pub async fn authors(&self, ids: &[String]) -> Result<HashMap<String, Speaker>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT message_id, author FROM message_authors WHERE message_id = ANY($1)",
                &[&ids],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get("message_id")?,
                    serde_json::from_value(r.try_get("author")?)?,
                ))
            })
            .collect()
    }

    pub async fn put_check_in(&self, c: &CheckIn) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO check_ins (id, at, agent_id, kind, prompt, reply) VALUES ($1,$2,$3,$4,$5,$6) \
                 ON CONFLICT (id) DO UPDATE SET reply = $6",
                &[&c.id, &c.at, &c.agent_id, &c.kind, &c.prompt, &c.reply],
            )
            .await
            .context("saving a check-in")?;
        Ok(())
    }

    /// The latest check-in of `agent_id` with a reply.
    pub async fn latest_check_in(&self, agent_id: &str) -> Result<Option<CheckIn>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                "SELECT * FROM check_ins WHERE agent_id = $1 AND reply IS NOT NULL \
                 ORDER BY at DESC, id DESC LIMIT 1",
                &[&agent_id],
            )
            .await?
            .as_ref()
            .map(check_in)
            .transpose()
    }

    /// How many wake-ups (not summaries) there have been since `since`.
    pub async fn wake_ups_since(&self, since: DateTime<Utc>) -> Result<i64> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*) AS n FROM check_ins WHERE kind = 'wake' AND at >= $1",
                &[&since],
            )
            .await?;
        Ok(row.try_get("n")?)
    }

    /// When each wake-up since `since` was, earliest first.
    pub async fn wakes_since(&self, since: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT at FROM check_ins WHERE kind = 'wake' AND at >= $1 ORDER BY at",
                &[&since],
            )
            .await?;
        rows.iter()
            .map(|r| r.try_get("at").map_err(Into::into))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::{Party, Suggestion};
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    /// An event's place in Desk and its message are kept; one written
    /// before Activity kept them reads with neither.
    #[tokio::test]
    async fn an_events_place_and_message_round_trip() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let allan = Party::agent("dw/allan", "allan");
            let room = uuid::Uuid::new_v4();
            let message = uuid::Uuid::new_v4();
            let placed = Event::new(Kind::Replied, allan.clone(), Some(allan.clone()), "replied")
                .at_place(
                    Some(crate::floor::Place::Room { room, thread: None }),
                    Some(message),
                );
            let plain = Event::new(Kind::Finished, allan.clone(), Some(allan), "finished");
            store.add_event(&placed).await.unwrap();
            store.add_event(&plain).await.unwrap();
            let client = store.pool().get().await.unwrap();
            let read = |id: String| {
                let client = &client;
                async move {
                    event(
                        &client
                            .query_one("SELECT * FROM activity WHERE id = $1", &[&id])
                            .await
                            .unwrap(),
                    )
                    .unwrap()
                }
            };
            let back = read(placed.id.clone()).await;
            assert_eq!(
                (back.place, back.message),
                (placed.place.clone(), Some(message))
            );
            let back = read(plain.id.clone()).await;
            assert_eq!((back.place, back.message), (None, None));
            assert_eq!(
                serde_json::to_value(&placed).unwrap()["place"],
                serde_json::json!({ "kind": "room", "room": room, "thread": null })
            );
        })
        .await;
    }

    #[tokio::test]
    async fn activity_pages_newest_first_by_filter_and_cursor() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let scout = Party::agent("scout", "Scout");
            let mut made = vec![];
            for (i, kind) in [Kind::Started, Kind::Question, Kind::Nudged, Kind::Finished]
                .into_iter()
                .enumerate()
            {
                let actor = if kind == Kind::Nudged {
                    Party::manager("desk", "Desk")
                } else {
                    scout.clone()
                };
                let mut e = Event::new(kind, actor, Some(scout.clone()), format!("event {i}"));
                e.at += chrono::Duration::seconds(i as i64);
                store.add_event(&e).await.unwrap();
                made.push(e);
            }
            let (all, more) = store.events(Filter::All, None, 3).await.unwrap();
            assert_eq!(
                all.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
                ["event 3", "event 2", "event 1"]
            );
            assert!(more);
            let (rest, more) = store
                .events(Filter::All, Some(&all[2].id), 3)
                .await
                .unwrap();
            assert_eq!(rest, vec![made[0].clone()]);
            assert!(!more);
            let (needs, _) = store.events(Filter::NeedsYou, None, 10).await.unwrap();
            assert_eq!(needs.len(), 1);
            assert_eq!(needs[0].kind, Kind::Question);
            let (by, _) = store.events(Filter::Manager, None, 10).await.unwrap();
            assert_eq!(by[0].kind, Kind::Nudged);
            let latest = store.latest_turn_events().await.unwrap();
            assert_eq!(latest["scout"].kind, Kind::Finished);
        })
        .await;
    }

    #[tokio::test]
    async fn what_can_need_you_is_found_however_much_came_after() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let scout = Party::agent("scout", "Scout");
            let other = Party::agent("other", "Other");
            let mut at = Utc::now() - chrono::Duration::days(3);
            let mut add = |p: &Party, kind, text: &str, approval: Option<&str>| {
                at += chrono::Duration::seconds(1);
                let mut e = Event::new(kind, p.clone(), Some(p.clone()), text);
                e.at = at;
                e.approval_id = approval.map(str::to_string);
                e
            };
            let events = [
                add(&scout, Kind::Question, "asked “A?”", None),
                add(&scout, Kind::NeedsYou, "wants to run ls", Some("a1")),
                add(&scout, Kind::NeedsYou, "wants to run rm", Some("a2")),
                add(&scout, Kind::Question, "asked “B?”", None),
                add(&other, Kind::Question, "asked “C?”", None),
            ];
            for e in &events {
                store.add_event(e).await.unwrap();
            }
            for i in 0..50 {
                store
                    .add_event(&add(
                        &scout,
                        Kind::Working,
                        &format!("is running {i}"),
                        None,
                    ))
                    .await
                    .unwrap();
            }
            let found = store
                .need_events(&["scout".to_string()], &["a1".to_string()])
                .await
                .unwrap();
            assert_eq!(
                found.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
                ["wants to run ls", "asked “B?”"]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn suggestions_extras_authors_and_check_ins_round_trip() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut s = Suggestion {
                id: crate::crew::new_id(),
                at: crate::crew::now(),
                text: "Restart it".into(),
                action: serde_json::json!({"tool": "crew.restart"}),
                state: SuggestionState::Open,
                subject: Some(Party::agent("scout", "Scout")),
                agent_id: "desk".into(),
            };
            store.put_suggestion(&s).await.unwrap();
            s.state = SuggestionState::Done;
            store.put_suggestion(&s).await.unwrap();
            assert_eq!(store.suggestion(&s.id).await.unwrap(), Some(s.clone()));
            assert_eq!(store.suggestions(10).await.unwrap().len(), 1);

            let extras = Extras {
                suggestions: vec![crate::crew::MentionedSuggestion {
                    id: s.id.clone(),
                    text: s.text.clone(),
                    state: SuggestionState::Open,
                }],
                ..Default::default()
            };
            store.put_extras("m1", "desk", &extras).await.unwrap();
            let got = store
                .extras(&["m1".to_string(), "m2".to_string()])
                .await
                .unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got["m1"], extras);

            store
                .put_author("m3", "scout", &Speaker::overseer("desk", "Atlas"))
                .await
                .unwrap();
            store
                .put_author("m4", "scout", &Speaker::you())
                .await
                .unwrap();
            let authors = store
                .authors(&["m3".to_string(), "m4".to_string(), "m5".to_string()])
                .await
                .unwrap();
            assert_eq!(authors.len(), 2);
            assert_eq!(authors["m3"], Speaker::overseer("desk", "Atlas"));
            assert_eq!(authors["m4"], Speaker::you());

            let mut c = CheckIn {
                id: crate::crew::new_id(),
                at: crate::crew::now(),
                agent_id: "desk".into(),
                kind: "wake".into(),
                prompt: "what happened".into(),
                reply: None,
            };
            store.put_check_in(&c).await.unwrap();
            assert_eq!(store.latest_check_in("desk").await.unwrap(), None);
            c.reply = Some("All quiet.".into());
            store.put_check_in(&c).await.unwrap();
            assert_eq!(store.latest_check_in("desk").await.unwrap(), Some(c));
            assert_eq!(
                store
                    .wake_ups_since(Utc::now() - chrono::Duration::hours(1))
                    .await
                    .unwrap(),
                1
            );
        })
        .await;
    }
}
