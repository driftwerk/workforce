//! What the boss owes the person, in Postgres (`boss_open_items`). Every
//! item opens through [`DeskStore::item_open`], keyed by its `dedupe_key`
//! (unique among the boss's items not closed): a second opening of the same
//! matter updates the item there and says so, so it is never posted twice.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::goal::{GoalAsk, Proposal};
use crate::open_item::{AskLayout, ClosedBy, NewItem, OpenItem, OpenKind, OpenState};
use crate::store::DeskStore;

const COLUMNS: &str = "id, boss, owner, kind, session, line_message, text, holding_up, \
     recommendation, options, generalizes, done_when, dedupe_key, opened_at, last_said_at, \
     said_count, changed_at, state, closed_at, closed_by, closed_why, raised_at, sessions, \
     prefer_scope, question, points, recommended, not_blocking, cant_decide, due_at, chased_at";

fn item(r: &Row) -> Result<OpenItem> {
    Ok(OpenItem {
        id: r.try_get(0)?,
        boss: r.try_get(1)?,
        owner: r.try_get(2)?,
        kind: OpenKind::parse(r.try_get(3)?)?,
        session: r.try_get(4)?,
        line_message: r.try_get(5)?,
        text: r.try_get(6)?,
        holding_up: r.try_get(7)?,
        recommendation: r.try_get(8)?,
        options: serde_json::from_value(r.try_get(9)?).unwrap_or_default(),
        generalizes: r.try_get(10)?,
        done_when: r.try_get(11)?,
        dedupe_key: r.try_get(12)?,
        opened_at: r.try_get(13)?,
        last_said_at: r.try_get(14)?,
        said_count: r.try_get(15)?,
        changed_at: r.try_get(16)?,
        state: OpenState::parse(r.try_get(17)?)?,
        closed_at: r.try_get(18)?,
        closed_by: r
            .try_get::<_, Option<&str>>(19)?
            .map(ClosedBy::parse)
            .transpose()?,
        closed_why: r.try_get(20)?,
        raised_at: r.try_get(21)?,
        sessions: r.try_get(22)?,
        prefer_scope: r.try_get(23)?,
        layout: AskLayout {
            question: r.try_get(24)?,
            points: serde_json::from_value(r.try_get(25)?).unwrap_or_default(),
            recommended: r.try_get(26)?,
            not_blocking: r.try_get(27)?,
        },
        cant_decide: r.try_get(28)?,
        due_at: r.try_get(29)?,
        chased_at: r.try_get(30)?,
    })
}

/// What opening an item did.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    /// Said to the person for the first time: say it (a new item, or the
    /// watcher's mark of one now said).
    New(OpenItem),
    /// Already said, and open (or parked): updated in place, and nothing
    /// new is said.
    Again(OpenItem),
}

impl Opened {
    pub fn item(&self) -> &OpenItem {
        match self {
            Self::New(i) | Self::Again(i) => i,
        }
    }
}

impl DeskStore {
    /// The scope an "always" answer to item `id` keeps its preference
    /// for, as the boss said when it asked (`all`, `group:<id>` or
    /// `repo:<owner/name>`); `None` when it said none.
    pub async fn item_prefer_scope(&self, id: Uuid) -> Result<Option<String>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT prefer_scope FROM boss_open_items WHERE id = $1",
                &[&id],
            )
            .await?
            .and_then(|r| r.get::<_, Option<String>>(0)))
    }

    /// Keeps `scope` as what an "always" answer to item `id` covers.
    pub async fn item_set_prefer_scope(&self, id: Uuid, scope: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_open_items SET prefer_scope = $2 WHERE id = $1",
                &[&id, &scope],
            )
            .await?;
        Ok(())
    }

    /// Keeps item `id` as the boss's question about plan `plan`'s goal,
    /// proposing `proposal`: proposed `at` when it proposes something
    /// other than it did.
    pub async fn item_set_goal(
        &self,
        id: Uuid,
        plan: Uuid,
        proposal: &Proposal,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_open_items SET plan = $2, proposal = $3, \
                 proposed_at = CASE WHEN proposal IS NOT DISTINCT FROM $3 AND plan = $2 \
                 THEN proposed_at ELSE $4 END, \
                 applied_at = CASE WHEN proposal IS NOT DISTINCT FROM $3 AND plan = $2 \
                 THEN applied_at END WHERE id = $1",
                &[&id, &plan, &serde_json::to_value(proposal)?, &at],
            )
            .await?;
        Ok(())
    }

    /// The boss's questions about plan `plan`'s goal whose proposal isn't
    /// applied yet, open or closed, newest proposed first: the person may
    /// answer one on its buttons, which closes it, or the boss close it with
    /// their answer before it applies it.
    pub async fn goal_asks(&self, plan: Uuid) -> Result<Vec<GoalAsk>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, proposal, proposed_at, closed_at, closed_by, closed_why \
                 FROM boss_open_items WHERE plan = $1 AND proposal IS NOT NULL \
                 AND proposed_at IS NOT NULL AND applied_at IS NULL \
                 ORDER BY proposed_at DESC, id",
                &[&plan],
            )
            .await?
            .iter()
            .map(|r| {
                let by: Option<String> = r.try_get("closed_by")?;
                let why: Option<String> = r.try_get("closed_why")?;
                let at: Option<DateTime<Utc>> = r.try_get("closed_at")?;
                let answered = match (by.as_deref(), why, at) {
                    (Some("person"), Some(why), Some(at)) => why
                        .strip_prefix(crate::goal::YOU_ANSWERED)
                        .map(|a| (at, a.to_string())),
                    _ => None,
                };
                Ok(GoalAsk {
                    item: r.try_get("id")?,
                    proposal: serde_json::from_value(r.try_get("proposal")?)
                        .context("reading what a question proposed")?,
                    at: r.try_get("proposed_at")?,
                    answered,
                })
            })
            .collect()
    }

    /// Notes that question `item`'s proposal was applied, and closes it as
    /// answered if it is still open: whether this did, so an answer is
    /// applied once.
    pub async fn ask_applied(&self, item: Uuid) -> Result<bool> {
        self.ask_settled(item, "Answered").await
    }

    /// [`Self::ask_applied`], closing it, if still open, as `why`.
    pub async fn ask_settled(&self, item: Uuid, why: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE boss_open_items SET applied_at = now(), changed_at = now(), \
                 state = 'closed', closed_at = COALESCE(closed_at, now()), \
                 closed_by = COALESCE(closed_by, 'person'), \
                 closed_why = COALESCE(closed_why, $2) \
                 WHERE id = $1 AND applied_at IS NULL",
                &[&item, &why],
            )
            .await?;
        Ok(n == 1)
    }

    /// Lets question `item` be answered again: what applying it was to do
    /// didn't happen.
    pub async fn ask_unapplied(&self, item: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_open_items SET applied_at = NULL WHERE id = $1",
                &[&item],
            )
            .await?;
        Ok(())
    }

    /// Opens `new`, or updates the item not closed with its key: in one
    /// transaction, the row locked, so two openings of one matter make one
    /// item and say it once.
    pub async fn item_open(&self, new: &NewItem) -> Result<Opened> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let now = crate::crew::now();
        let options = serde_json::to_value(&new.options)?;
        let points = serde_json::to_value(&new.layout.points)?;
        let select = format!(
            "SELECT {COLUMNS} FROM boss_open_items \
             WHERE boss = $1 AND dedupe_key = $2 AND state <> 'closed' FOR UPDATE"
        );
        let mut was = tx
            .query_opt(&select, &[&new.boss, &new.dedupe_key])
            .await?
            .as_ref()
            .map(item)
            .transpose()?;
        if was.is_none() {
            let said_at = new.said.then_some(now);
            let said = i32::from(new.said);
            let inserted = tx
                .query_opt(
                    &format!(
                        "INSERT INTO boss_open_items (id, boss, owner, kind, session, line_message, \
                         text, holding_up, recommendation, options, generalizes, done_when, \
                         dedupe_key, opened_at, last_said_at, said_count, changed_at, state, \
                         sessions, question, points, recommended, not_blocking, cant_decide, \
                         due_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                         $16, $14, 'open', $17, $18, $19, $20, $21, $22, $23) \
                         ON CONFLICT (boss, dedupe_key) WHERE state <> 'closed' DO NOTHING \
                         RETURNING {COLUMNS}"
                    ),
                    &[
                        &Uuid::new_v4(),
                        &new.boss,
                        &crate::rooms::YOU,
                        &new.kind.as_str(),
                        &new.session,
                        &new.line_message,
                        &new.text,
                        &new.holding_up,
                        &new.recommendation,
                        &options,
                        &new.generalizes,
                        &new.done_when,
                        &new.dedupe_key,
                        &now,
                        &said_at,
                        &said,
                        &new.sessions,
                        &new.layout.question,
                        &points,
                        &new.layout.recommended,
                        &new.layout.not_blocking,
                        &new.cant_decide,
                        &new.due_at,
                    ],
                )
                .await?;
            match inserted {
                Some(r) => {
                    let i = item(&r)?;
                    tx.commit().await?;
                    return Ok(match new.said {
                        true => Opened::New(i),
                        false => Opened::Again(i),
                    });
                }
                // Opened by another just now: updated as it is below.
                None => {
                    was = tx
                        .query_opt(&select, &[&new.boss, &new.dedupe_key])
                        .await?
                        .as_ref()
                        .map(item)
                        .transpose()?;
                }
            }
        }
        let Some(was) = was else {
            eyre::bail!(
                "an open item with key {} vanished as it opened",
                new.dedupe_key
            );
        };
        // What it says now. The watcher's mark keeps the words of the item
        // it marks; a newer saying replaces them.
        let text = match new.said || !was.said() {
            true => new.text.clone(),
            false => was.text.clone(),
        };
        // The answer recommended: the one given, else the one kept while it
        // is still among the options (new options can drop it).
        let recommended = new.layout.recommended.clone().or_else(|| {
            was.layout.recommended.clone().filter(|r| {
                new.options.is_empty()
                    || crate::open_item::matching_option(&new.options, r).is_some()
            })
        });
        let changed = text != was.text
            || new.sessions.iter().any(|s| !was.sessions.contains(s))
            || (was.done_when.is_none() && new.done_when.is_some())
            || (!new.options.is_empty() && new.options != was.options)
            || (new.holding_up.is_some() && new.holding_up != was.holding_up)
            || (new.recommendation.is_some() && new.recommendation != was.recommendation)
            // The layout as it merges below: each part given replaces its own.
            || (new.layout.question.is_some() && new.layout.question != was.layout.question)
            || (!new.layout.points.is_empty() && new.layout.points != was.layout.points)
            || recommended != was.layout.recommended
            || (new.layout.not_blocking.is_some()
                && new.layout.not_blocking != was.layout.not_blocking)
            || (new.cant_decide.is_some() && new.cant_decide != was.cant_decide)
            || (new.due_at.is_some() && new.due_at != was.due_at);
        let first_said = new.said && !was.said();
        let r = tx
            .query_one(
                &format!(
                    "UPDATE boss_open_items SET text = $2, \
                     holding_up = COALESCE($3, holding_up), \
                     recommendation = COALESCE($4, recommendation), \
                     options = CASE WHEN $5 = '[]'::jsonb THEN options ELSE $5 END, \
                     line_message = CASE WHEN $9 = 'promise' THEN COALESCE($6, line_message) \
                     ELSE COALESCE(line_message, $6) END, \
                     generalizes = COALESCE(generalizes, $7), \
                     kind = CASE WHEN $8 THEN $9 ELSE kind END, \
                     session = COALESCE(session, $10), \
                     said_count = CASE WHEN $8 THEN 1 ELSE said_count END, \
                     last_said_at = CASE WHEN $8 THEN $11 ELSE last_said_at END, \
                     changed_at = CASE WHEN $12 OR $8 THEN $11 ELSE changed_at END, \
                     sessions = ARRAY(SELECT DISTINCT unnest(sessions || $13::uuid[])), \
                     question = COALESCE($14, question), \
                     points = CASE WHEN $15 = '[]'::jsonb THEN points ELSE $15 END, \
                     recommended = $16, \
                     not_blocking = COALESCE($17, not_blocking), \
                     cant_decide = COALESCE($18, cant_decide), \
                     done_when = COALESCE(done_when, $19), \
                     due_at = COALESCE($20, due_at), \
                     chased_at = CASE WHEN $20::timestamptz IS NULL THEN chased_at END \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ),
                &[
                    &was.id,
                    &text,
                    &new.holding_up,
                    &new.recommendation,
                    &options,
                    &new.line_message,
                    &new.generalizes,
                    &first_said,
                    &new.kind.as_str(),
                    &new.session,
                    &now,
                    &changed,
                    &new.sessions,
                    &new.layout.question,
                    &points,
                    &recommended,
                    &new.layout.not_blocking,
                    &new.cant_decide,
                    &new.done_when,
                    &new.due_at,
                ],
            )
            .await?;
        let i = item(&r)?;
        tx.commit().await?;
        Ok(match first_said {
            true => Opened::New(i),
            false => Opened::Again(i),
        })
    }

    pub async fn open_item(&self, id: Uuid) -> Result<Option<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM boss_open_items WHERE id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(item)
            .transpose()
    }

    /// Whichever of `ids` are kept.
    pub async fn open_items_of(&self, ids: &[Uuid]) -> Result<Vec<OpenItem>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {COLUMNS} FROM boss_open_items WHERE id = ANY($1)"),
                &[&ids],
            )
            .await?
            .iter()
            .map(item)
            .collect()
    }

    /// `boss`'s items not closed, oldest first; every boss's with `None`.
    pub async fn items_not_closed(&self, boss: Option<&str>) -> Result<Vec<OpenItem>> {
        let client = self.pool().get().await?;
        let rows = match boss {
            Some(b) => {
                client
                    .query(
                        &format!(
                            "SELECT {COLUMNS} FROM boss_open_items \
                             WHERE boss = $1 AND state <> 'closed' ORDER BY opened_at, id"
                        ),
                        &[&b],
                    )
                    .await?
            }
            None => {
                client
                    .query(
                        &format!(
                            "SELECT {COLUMNS} FROM boss_open_items \
                             WHERE state <> 'closed' ORDER BY opened_at, id"
                        ),
                        &[],
                    )
                    .await?
            }
        };
        rows.iter().map(item).collect()
    }

    /// `boss`'s items closed since `since`, newest first, at most `limit`.
    pub async fn items_closed_since(
        &self,
        boss: &str,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_open_items WHERE boss = $1 AND state = 'closed' \
                     AND closed_at >= $2 AND said_count > 0 ORDER BY closed_at DESC LIMIT $3"
                ),
                &[&boss, &since, &limit],
            )
            .await?
            .iter()
            .map(item)
            .collect()
    }

    /// Holds the session question `message` until the person has left its
    /// session: kept across restarts.
    pub async fn held_question_put(&self, message: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO boss_held_questions (line_message) VALUES ($1) \
                 ON CONFLICT (line_message) DO NOTHING",
                &[&message],
            )
            .await?;
        Ok(())
    }

    /// Lets the session question `message` go: said, theirs, or no longer
    /// waiting.
    pub async fn held_question_drop(&self, message: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM boss_held_questions WHERE line_message = $1",
                &[&message],
            )
            .await?;
        Ok(())
    }

    /// The session questions held, oldest first.
    pub async fn held_questions(&self) -> Result<Vec<Uuid>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT line_message FROM boss_held_questions ORDER BY held_at",
                &[],
            )
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }

    /// Whether `boss` ever had an item keyed `key`, closed or not.
    pub async fn item_ever_keyed(&self, boss: &str, key: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT 1 FROM boss_open_items WHERE boss = $1 AND dedupe_key = $2 LIMIT 1",
                &[&boss, &key],
            )
            .await?
            .is_some())
    }

    /// When the person last closed `boss`'s item keyed `key`, and why
    /// ("You answered: …", "You dropped it."), if they did.
    pub async fn item_last_by_person(
        &self,
        boss: &str,
        key: &str,
    ) -> Result<Option<(DateTime<Utc>, String)>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT closed_at, coalesce(closed_why, '') FROM boss_open_items \
                 WHERE boss = $1 AND dedupe_key = $2 AND state = 'closed' AND closed_by = 'person' \
                 AND closed_at IS NOT NULL ORDER BY closed_at DESC LIMIT 1",
                &[&boss, &key],
            )
            .await?
            .map(|r| (r.get::<_, DateTime<Utc>>(0), r.get::<_, String>(1))))
    }

    /// The item not closed for the session question `message`, if one.
    pub async fn item_of_line(&self, message: Uuid) -> Result<Option<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM boss_open_items \
                     WHERE line_message = $1 AND state <> 'closed' LIMIT 1"
                ),
                &[&message],
            )
            .await?
            .as_ref()
            .map(item)
            .transpose()
    }

    /// Open promises whose chase is due by `now`.
    pub async fn items_due(&self, now: DateTime<Utc>) -> Result<Vec<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_open_items \
                     WHERE due_at <= $1 AND kind = 'promise' AND state = 'open' \
                     ORDER BY due_at, id"
                ),
                &[&now],
            )
            .await?
            .iter()
            .map(item)
            .collect()
    }

    /// `id`, read due at `was`, was chased: its boss told to get it at
    /// `boss` (none: the person told), and due again at `next` (none: never
    /// again). Whether this caller moved it: an item closed, chased or
    /// asked afresh since it was read is left as it is.
    pub async fn item_chased(
        &self,
        id: Uuid,
        was: DateTime<Utc>,
        boss: Option<DateTime<Utc>>,
        next: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE boss_open_items SET chased_at = COALESCE($3, chased_at), due_at = $4 \
                 WHERE id = $1 AND due_at = $2 AND state <> 'closed'",
                &[&id, &was, &boss, &next],
            )
            .await?;
        Ok(n == 1)
    }

    /// Closes item `id` (open or parked) as `by` says, for `why`: the item
    /// as closed, or `None` when it was already closed.
    pub async fn item_close(&self, id: Uuid, by: ClosedBy, why: &str) -> Result<Option<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_open_items SET state = 'closed', closed_at = now(), changed_at = now(), \
                     closed_by = $2, closed_why = $3 WHERE id = $1 AND state <> 'closed' \
                     RETURNING {COLUMNS}"
                ),
                &[&id, &by.as_str(), &why],
            )
            .await?
            .as_ref()
            .map(item)
            .transpose()
    }

    /// Closes the item of the session question `message`, if one is open:
    /// the item as closed.
    pub async fn item_close_of_line(
        &self,
        message: Uuid,
        by: ClosedBy,
        why: &str,
    ) -> Result<Option<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_open_items SET state = 'closed', closed_at = now(), changed_at = now(), \
                     closed_by = $2, closed_why = $3 \
                     WHERE line_message = $1 AND state <> 'closed' RETURNING {COLUMNS}"
                ),
                &[&message, &by.as_str(), &why],
            )
            .await?
            .as_ref()
            .map(item)
            .transpose()
    }

    /// Closes every boss's item not closed with `key` (a request's, as it
    /// is answered): the items as closed.
    pub async fn item_close_of_key(
        &self,
        key: &str,
        by: ClosedBy,
        why: &str,
    ) -> Result<Vec<OpenItem>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "UPDATE boss_open_items SET state = 'closed', closed_at = now(), changed_at = now(), \
                     closed_by = $2, closed_why = $3 \
                     WHERE dedupe_key = $1 AND state <> 'closed' RETURNING {COLUMNS}"
                ),
                &[&key, &by.as_str(), &why],
            )
            .await?
            .iter()
            .map(item)
            .collect()
    }

    /// Closes every boss's item not closed whose key starts with `prefix`
    /// (the stalls a restart forgot): how many.
    pub async fn item_close_of_key_prefix(
        &self,
        prefix: &str,
        by: ClosedBy,
        why: &str,
    ) -> Result<u64> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE boss_open_items SET state = 'closed', closed_at = now(), changed_at = now(), \
                 closed_by = $2, closed_why = $3 \
                 WHERE left(dedupe_key, length($1)) = $1 AND state <> 'closed'",
                &[&prefix, &by.as_str(), &why],
            )
            .await?)
    }

    /// Opens item `id` again, closed by a step that then didn't take: false
    /// when it isn't closed, or another of its key is open by now.
    pub async fn item_reopen(&self, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE boss_open_items SET state = 'open', closed_at = NULL, closed_by = NULL, \
                 closed_why = NULL, changed_at = now() WHERE id = $1 AND state = 'closed' \
                 AND NOT EXISTS (SELECT 1 FROM boss_open_items o WHERE o.boss = boss_open_items.boss \
                 AND o.dedupe_key = boss_open_items.dedupe_key AND o.state <> 'closed')",
                &[&id],
            )
            .await?;
        Ok(n == 1)
    }

    /// Item `id` was put in a message at `at`: counted, and parked when
    /// `park` says so.
    pub async fn item_said(&self, id: Uuid, at: DateTime<Utc>, park: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_open_items SET said_count = said_count + 1, last_said_at = $2, \
                 state = CASE WHEN $3 AND state = 'open' THEN 'parked' ELSE state END, \
                 changed_at = CASE WHEN $3 THEN $2 ELSE changed_at END \
                 WHERE id = $1 AND state <> 'closed'",
                &[&id, &at, &park],
            )
            .await?;
        Ok(())
    }

    /// The watcher put `key` to `boss` at `at`: kept on the item of that
    /// key, opened as the watcher's mark (never said to the person) when
    /// there is none.
    pub async fn item_raise(&self, mark: &NewItem, at: DateTime<Utc>) -> Result<()> {
        let opened = self.item_open(mark).await?;
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_open_items SET raised_at = $2 WHERE id = $1",
                &[&opened.item().id, &at],
            )
            .await?;
        Ok(())
    }

    /// When each key of `boss`'s items not closed was last put to it by
    /// the watcher, since `since`.
    pub async fn items_raised(
        &self,
        boss: &str,
        since: DateTime<Utc>,
    ) -> Result<HashMap<String, DateTime<Utc>>> {
        let client = self.pool().get().await?;
        let mut out = HashMap::new();
        for r in client
            .query(
                "SELECT dedupe_key, raised_at FROM boss_open_items \
                 WHERE boss = $1 AND state <> 'closed' AND raised_at >= $2",
                &[&boss, &since],
            )
            .await?
        {
            out.insert(r.try_get(0)?, r.try_get(1)?);
        }
        Ok(out)
    }
}
