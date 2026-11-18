//! `crew.find_tickets`: the boss finds tickets on its boards by what they
//! know of them (their column, and on a synced board their issue's labels,
//! type, epic, who has it there and when it was made), to take them on in
//! a plan with `crew.plan` (`adopt`), or to answer the person. Each says
//! whether it can be taken on: who has it, or which plan.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use workforce_desk::board::TicketState;
use workforce_desk::crew::{self, PartyKind};
use workforce_desk::find::{self, TicketFind, FIND_MAX};
use workforce_desk::Found;

use super::tools::{cell, Bound};
use super::watcher::reported;
use super::CrewHub;

pub const NAME: &str = "crew.find_tickets";

pub const DESCRIPTION: &str = "Find tickets on your boards to take on in a plan, or to answer the \
person: by board (its name or prefix), state, label, type, epic, created_since and assignee, using \
what each synced Linear or Jira issue says of itself, newest first. Each comes with its key, \
whether it can be taken on now (free) or who or which plan has it, and its link. On Linear a bug \
is usually a label (label: Bug), on Jira a type (type: Bug); try the other when one finds \
nothing. For \"plan fixing the recent bugs on Mantis\": find them here, then crew.plan with each \
free one as {adopt: its key, folder}, and the person approves it.";

/// The default and the most tickets one call gives.
const SHOWN: usize = 30;

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "board": { "type": "string", "description": "One of your boards, by its name or its prefix (like MAN); all of yours when not said" },
        "state": { "type": "array", "items": { "type": "string", "enum": ["backlog", "todo", "doing", "review", "done", "dropped"] },
            "description": "Its columns; the open ones (backlog, todo, doing, review) when not said" },
        "label": { "type": "string", "description": "A label its issue has, like Bug" },
        "type": { "type": "string", "description": "Its issue's type, like Bug or Story" },
        "epic": { "type": "string", "description": "The epic or parent issue it is under, by key or title" },
        "created_since": { "type": "string", "description": "Made on or after: a date (2026-11-01) or a span back from now (7d, 2w, 24h)" },
        "assignee": { "type": "string", "description": "Who has it: me (the person), none (no one), or a name" },
        "q": { "type": "string", "description": "Words in its title, or its key" },
        "limit": { "type": "integer", "minimum": 1, "maximum": FIND_MAX, "description": "At most this many; 30 when not said" }
    }})
}

/// `created_since` as a time: a date (midnight UTC), an RFC 3339 time, or
/// a span back from `now` (`7d`, `2w`, `24h`).
pub(crate) fn since_of(said: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let s = said.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    let at = s.char_indices().last().map_or(0, |(i, _)| i);
    let (n, unit) = s.split_at(at);
    let n: i64 = n.trim().parse().map_err(|_| bad_since(said))?;
    let span = match unit.to_ascii_lowercase().as_str() {
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => return Err(bad_since(said)),
    };
    Ok(now - span)
}

fn bad_since(said: &str) -> String {
    format!(
        "created_since {:?} doesn't read: give a date like 2026-11-01, or a span like 7d.",
        crew::one_line(said, 40)
    )
}

/// What a found ticket says to the boss.
fn row(f: &Found) -> Value {
    let t = &f.view.ticket;
    let e = t.external.as_ref();
    let held = match (&t.assignee, &f.plan) {
        (Some(a), _) if a.kind == PartyKind::You => "the person has it".to_string(),
        (Some(a), _) => format!("{} has it", a.name),
        (None, Some((_, goal))) => format!("in the plan “{}”", crew::one_line(goal, 60)),
        (None, None) if !matches!(t.state, TicketState::Backlog | TicketState::Todo) => {
            format!("in {}", t.state.title())
        }
        (None, None) => "free".to_string(),
    };
    json!({
        "key": f.view.key,
        "title": t.said_title(),
        "board": f.board_name,
        "state": t.state.as_str(),
        "type": e.and_then(|e| e.details.facts.kind.clone()),
        "labels": e.map(|e| e.details.labels.clone()).unwrap_or_default(),
        "epic": e.and_then(|e| e.details.facts.epic.as_ref().or(e.details.facts.parent.as_ref())).map(|r| r.key.clone()),
        "assignee_there": find::assignee_there(t),
        "created": find::created(t).date_naive().to_string(),
        "can_take_on": held == "free",
        "held": held,
        "url": e.map(|e| e.url.clone()),
    })
}

impl CrewHub {
    /// `crew.find_tickets`, for the boss `b`: of the boards it sees only.
    pub(super) async fn find_tickets_tool(
        &self,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let str_of = |k: &str| {
            input
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let states = match input.get("state") {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => {
                vec![TicketState::parse(&s.to_lowercase()).map_err(|_| {
                    format!("{s:?} isn't a column: backlog, todo, doing, review, done or dropped.")
                })?]
            }
            Some(Value::Array(list)) => list
                .iter()
                .map(|v| {
                    let s = v.as_str().unwrap_or_default();
                    TicketState::parse(&s.to_lowercase()).map_err(|_| {
                        format!(
                            "{s:?} isn't a column: backlog, todo, doing, review, done or dropped."
                        )
                    })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("state is a list of columns, like [\"todo\"].".into()),
        };
        let created_since = match str_of("created_since") {
            Some(s) => Some(since_of(&s, crew::now())?),
            None => None,
        };
        let f = TicketFind {
            board: str_of("board"),
            states,
            label: str_of("label"),
            kind: str_of("type"),
            epic: str_of("epic"),
            created_since,
            assignee: str_of("assignee"),
            q: str_of("q"),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(SHOWN, |n| (n as usize).clamp(1, FIND_MAX));
        let seen = &b.seen;
        let (found, total) = self
            .desk
            .find_tickets(&f, |g| seen.sees(g), limit)
            .await
            .map_err(|e| match e {
                workforce_desk::DeskError::Invalid(why) => why,
                e => {
                    tracing::warn!(error = %e, "finding tickets for crew.find_tickets failed");
                    "The boards could not be read just now.".to_string()
                }
            })?;
        let rows: Vec<Value> = found.iter().map(row).collect();
        let free = rows
            .iter()
            .filter(|r| r["can_take_on"] == json!(true))
            .count();
        let mut table =
            String::from("| Key | Title | Board | State | Type | Labels | Has it |\n|---|---|---|---|---|---|---|\n");
        for (f, r) in found.iter().zip(&rows) {
            table.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                f.view.key.as_deref().unwrap_or("-"),
                cell(&f.view.ticket.said_title()),
                cell(&f.board_name),
                f.view.ticket.state.title(),
                cell(r["type"].as_str().unwrap_or("")),
                cell(
                    &f.view
                        .ticket
                        .external
                        .as_ref()
                        .map(|e| e.details.labels.join(", "))
                        .unwrap_or_default()
                ),
                cell(r["held"].as_str().unwrap_or("")),
            ));
        }
        let note = match (total, free) {
            (0, _) => "None found. On Linear a bug is usually a label, on Jira a type: try the other, or fewer filters.".to_string(),
            (n, _) if n > found.len() => format!(
                "{n} found; the newest {} shown. Narrow it (created_since, label) or raise limit.",
                found.len()
            ),
            (_, 0) => "None of them can be taken on now: someone or a plan has each.".to_string(),
            _ => "Take the free ones on with crew.plan: each as {adopt: its key, folder}.".to_string(),
        };
        Ok(json!({
            "found": total,
            "shown": found.len(),
            "free": free,
            "tickets": reported(&serde_json::to_string(&rows).unwrap_or_default(), "the board's tickets"),
            "table": reported(&table, "the board's tickets"),
            "note": note,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_since_reads_a_date_a_time_or_a_span() {
        let now: DateTime<Utc> = "2026-11-18T12:00:00Z".parse().unwrap();
        assert_eq!(
            since_of("2026-11-01", now).unwrap(),
            "2026-11-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(since_of("7d", now).unwrap(), now - Duration::days(7));
        assert_eq!(since_of("2W", now).unwrap(), now - Duration::weeks(2));
        assert_eq!(since_of("24h", now).unwrap(), now - Duration::hours(24));
        assert_eq!(
            since_of("2026-11-17T08:00:00+01:00", now).unwrap(),
            "2026-11-17T07:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert!(since_of("recently", now)
            .unwrap_err()
            .contains("doesn't read"));
        assert!(since_of("7y", now).is_err());
        assert!(since_of("7д", now).is_err(), "never a panic");
    }
}
