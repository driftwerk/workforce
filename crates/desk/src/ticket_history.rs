//! What happened to a synced ticket's issue there: Jira's changelog, or
//! Linear's history, read with its comments when the ticket is opened, and
//! kept as last read. Shown beside the comments, never pushed.
//!
//! Who made each change is said as:
//! - `desk`: the account Desk writes with, where that is told apart from
//!   the person: a shared key's own account (the hub's identity tool names
//!   it). On the person's own key Desk writes as them, so their changes are
//!   theirs (`remote`), whoever made them;
//! - `github`: GitHub's app there;
//! - `remote`: anyone else there, by their name.
//!
//! Every value is someone else's words: unwrapped from the hub's fence,
//! kept to one short line, and never read by a model.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::board::System;
use crate::board_sync::{remote_time, unfence};
use crate::store::DeskStore;

/// The most of an issue's history kept: its newest.
pub const HISTORY_MAX: usize = 100;
/// The longest value an event keeps.
const VALUE_MAX: usize = 120;

/// Who made a change there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryBy {
    Remote,
    Github,
    Desk,
}

/// What a change there was to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryKind {
    Created,
    State,
    Assignee,
    Priority,
    Sprint,
    Points,
    Parent,
    Link,
    Field,
}

/// One change there, as Desk shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub by: HistoryBy,
    pub kind: HistoryKind,
    /// The field it changed, for a `field` change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl HistoryEvent {
    fn new(at: DateTime<Utc>, who: &(HistoryBy, Option<String>), kind: HistoryKind) -> Self {
        Self {
            at,
            actor: who.1.clone(),
            by: who.0,
            kind,
            field: None,
            from: None,
            to: None,
        }
    }

    fn change(mut self, from: Option<String>, to: Option<String>) -> Self {
        self.from = from;
        self.to = to;
        self
    }

    fn field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }
}

/// A value there as one short line: unwrapped, and none when empty.
fn text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => unfence(s).0,
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let s = crate::crew::one_line(s.trim(), VALUE_MAX);
    (!s.is_empty()).then_some(s)
}

/// The call that reads `key`'s history: Jira's issue with its changelog,
/// or Linear's history tool.
pub fn history_call(system: System, key: &str) -> (&'static str, Value) {
    match system {
        System::Jira => (
            "jira.get_issue",
            serde_json::json!({ "issue_key": key, "changelog": true }),
        ),
        System::Linear => (
            "linear.issue_history",
            serde_json::json!({ "identifier": key, "limit": HISTORY_MAX }),
        ),
    }
}

/// The account Desk writes with, from the hub's identity tool's answer
/// (`jira.myself`, `linear.viewer`), when the key is a shared one: only
/// then are its changes told apart from the person's.
pub fn writer_of(system: System, answer: &Value) -> Option<String> {
    if answer["personal"].as_bool() != Some(false) {
        return None;
    }
    let id = match system {
        System::Jira => &answer["account_id"],
        System::Linear => &answer["id"],
    };
    id.as_str()
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .map(str::to_string)
}

/// Who made a change: Desk, when it was the account Desk writes with;
/// GitHub's app; or whoever there.
fn who(
    id: Option<&str>,
    name: Option<String>,
    app: bool,
    writer: Option<&str>,
) -> (HistoryBy, Option<String>) {
    if id.is_some() && id == writer {
        return (HistoryBy::Desk, name);
    }
    let github = name
        .as_deref()
        .is_some_and(|n| n.to_lowercase().contains("github"));
    match app && github {
        true => (HistoryBy::Github, name),
        false => (HistoryBy::Remote, name),
    }
}

/// The newest [`HISTORY_MAX`], oldest first.
fn newest(mut events: Vec<HistoryEvent>) -> Vec<HistoryEvent> {
    events.sort_by_key(|e| e.at);
    let from = events.len().saturating_sub(HISTORY_MAX);
    events.split_off(from)
}

// ── Jira ─────────────────────────────────────────────────────────────────

/// Changelog fields that say nothing to the person: the board's rank, and
/// time logged.
const JIRA_NOISE: [&str; 6] = [
    "rank",
    "worklogid",
    "timespent",
    "timeestimate",
    "timeoriginalestimate",
    "worklogtimespent",
];

/// The issue key a Jira link's words name ("This issue blocks ACME-2").
fn linked_key(words: &str) -> String {
    words.split_whitespace().last().unwrap_or(words).to_string()
}

/// One changelog item as an event, or none for noise.
fn jira_item(
    at: DateTime<Utc>,
    who: &(HistoryBy, Option<String>),
    item: &Value,
) -> Option<HistoryEvent> {
    let field = text(&item["field"])?;
    let lower = field.to_lowercase();
    if JIRA_NOISE.contains(&lower.as_str()) {
        return None;
    }
    let (from, to) = (text(&item["from"]), text(&item["to"]));
    let e = |kind| HistoryEvent::new(at, who, kind);
    Some(match lower.as_str() {
        "status" => e(HistoryKind::State).change(from, to),
        "assignee" => e(HistoryKind::Assignee).change(from, to),
        "priority" => e(HistoryKind::Priority).change(from, to),
        "sprint" => e(HistoryKind::Sprint).change(from, to),
        "story points" | "story point estimate" => e(HistoryKind::Points).change(from, to),
        "parent" | "issueparentassociation" | "epic link" => {
            e(HistoryKind::Parent).change(from, to)
        }
        "link" => match to {
            Some(to) => e(HistoryKind::Link).change(None, Some(linked_key(&to))),
            None => e(HistoryKind::Field)
                .field("link")
                .change(from.as_deref().map(linked_key), None),
        },
        // A whole text: that it changed, not what to.
        "description" | "environment" => e(HistoryKind::Field).field(&field),
        _ => e(HistoryKind::Field).field(&field).change(from, to),
    })
}

/// A `jira.get_issue` answer's history: when it was made (by its reporter),
/// and its `changelog`, newest [`HISTORY_MAX`] kept. `writer` is the account
/// Desk writes with ([`writer_of`]). None when the answer has no changelog
/// (an older hub): what was kept stays.
pub fn jira_history(answer: &Value, writer: Option<&str>) -> Option<Vec<HistoryEvent>> {
    let histories = answer["changelog"]["histories"].as_array()?;
    let mut events = Vec::new();
    if let Some(at) = remote_time(&answer["created"]) {
        let r = &answer["reporter"];
        let who = who(
            r["account_id"].as_str(),
            text(&r["display_name"]),
            false,
            writer,
        );
        events.push(HistoryEvent::new(at, &who, HistoryKind::Created));
    }
    for h in histories {
        let Some(at) = remote_time(&h["created"]) else {
            continue;
        };
        let who = who(
            h["author_account_id"].as_str(),
            text(&h["author"]),
            h["author_account_type"].as_str() == Some("app"),
            writer,
        );
        events.extend(
            h["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| jira_item(at, &who, i)),
        );
    }
    Some(newest(events))
}

// ── Linear ───────────────────────────────────────────────────────────────

/// Linear's priority number in words.
fn linear_priority(v: &Value) -> Option<String> {
    let name = match v.as_u64()? {
        0 => "No priority",
        1 => "Urgent",
        2 => "High",
        3 => "Medium",
        4 => "Low",
        _ => return None,
    };
    Some(name.into())
}

/// A Linear cycle by its name, or its number.
fn cycle(v: &Value) -> Option<String> {
    text(&v["name"]).or_else(|| v["number"].as_u64().map(|n| format!("Cycle {n}")))
}

/// A pull request's number, `#5850`, from its address on GitHub.
fn pr_number(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let n = rest.split("/pull/").nth(1)?.split(['/', '?', '#']).next()?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| format!("#{n}"))
}

/// The changes one Linear history entry made.
fn linear_entry(n: &Value, writer: Option<&str>) -> Vec<HistoryEvent> {
    let Some(at) = remote_time(&n["createdAt"]) else {
        return vec![];
    };
    let bot = n["botActor"].is_object().then_some(&n["botActor"]);
    let who = match (n["actor"].is_object(), bot) {
        (true, _) => who(
            n["actor"]["id"].as_str(),
            text(&n["actor"]["name"]),
            false,
            writer,
        ),
        (false, Some(b)) => {
            let github = b["type"].as_str() == Some("github");
            match github {
                true => (HistoryBy::Github, text(&b["name"])),
                false => who(None, text(&b["name"]), true, writer),
            }
        }
        (false, None) => (HistoryBy::Remote, None),
    };
    let e = |kind| HistoryEvent::new(at, &who, kind);
    let pair = |a: &str, b: &str, f: fn(&Value) -> Option<String>| {
        let (from, to) = (f(&n[a]), f(&n[b]));
        (from.is_some() || to.is_some()).then_some((from, to))
    };
    let name = |v: &Value| text(&v["name"]);
    let key = |v: &Value| text(&v["identifier"]);
    let mut out = Vec::new();
    if let Some((f, t)) = pair("fromState", "toState", name) {
        out.push(e(HistoryKind::State).change(f, t));
    }
    if let Some((f, t)) = pair("fromAssignee", "toAssignee", name) {
        out.push(e(HistoryKind::Assignee).change(f, t));
    }
    if let Some((f, t)) = pair("fromPriority", "toPriority", linear_priority) {
        if f != t {
            out.push(e(HistoryKind::Priority).change(f, t));
        }
    }
    if let Some((f, t)) = pair("fromEstimate", "toEstimate", text) {
        if f != t {
            out.push(e(HistoryKind::Points).change(f, t));
        }
    }
    if let Some((f, t)) = pair("fromCycle", "toCycle", cycle) {
        out.push(e(HistoryKind::Sprint).change(f, t));
    }
    if let Some((f, t)) = pair("fromParent", "toParent", key) {
        out.push(e(HistoryKind::Parent).change(f, t));
    }
    if let Some((f, t)) = pair("fromProject", "toProject", name) {
        out.push(e(HistoryKind::Field).field("project").change(f, t));
    }
    if let Some((f, t)) = pair("fromTitle", "toTitle", text) {
        out.push(e(HistoryKind::Field).field("title").change(f, t));
    }
    if let Some((f, t)) = pair("fromDueDate", "toDueDate", text) {
        out.push(e(HistoryKind::Field).field("due date").change(f, t));
    }
    let names = |k: &str| {
        let all: Vec<String> = n[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(name)
            .collect();
        (!all.is_empty()).then(|| all.join(", "))
    };
    let (removed, added) = (names("removedLabels"), names("addedLabels"));
    if removed.is_some() || added.is_some() {
        out.push(e(HistoryKind::Field).field("labels").change(removed, added));
    }
    for r in n["relationChanges"].as_array().into_iter().flatten() {
        if let Some(k) = key(r) {
            out.push(e(HistoryKind::Link).change(None, Some(k)));
        }
    }
    let a = &n["attachment"];
    if a.is_object() {
        let pr = a["url"].as_str().and_then(pr_number);
        if let Some(to) = pr.clone().or_else(|| text(&a["title"])) {
            let mut link = e(HistoryKind::Link).change(None, Some(to));
            // GitHub's own link, with no one there named as making it.
            if pr.is_some() && a["sourceType"].as_str() == Some("github") && who.1.is_none() {
                link.by = HistoryBy::Github;
            }
            out.push(link);
        }
    }
    if n["updatedDescription"].as_bool() == Some(true) {
        out.push(e(HistoryKind::Field).field("description"));
    }
    out
}

/// A `linear.issue_history` answer's history: when it was made (by its
/// creator), and each change, newest [`HISTORY_MAX`] kept. `writer` is the
/// account Desk writes with ([`writer_of`]). None when the answer lists no
/// history.
pub fn linear_history(answer: &Value, writer: Option<&str>) -> Option<Vec<HistoryEvent>> {
    let nodes = answer["history"].as_array()?;
    let mut events = Vec::new();
    if let Some(at) = remote_time(&answer["created_at"]) {
        let c = &answer["creator"];
        let who = who(c["id"].as_str(), text(&c["name"]), false, writer);
        events.push(HistoryEvent::new(at, &who, HistoryKind::Created));
    }
    events.extend(nodes.iter().flat_map(|n| linear_entry(n, writer)));
    Some(newest(events))
}

// ── In Postgres ──────────────────────────────────────────────────────────

impl DeskStore {
    /// `ticket`'s issue's history as last read, oldest first; none when it
    /// never was.
    pub async fn history_of(&self, ticket: Uuid) -> Result<Option<Vec<HistoryEvent>>> {
        let client = self.pool().get().await?;
        let Some(row) = client
            .query_opt(
                "SELECT events FROM ticket_history WHERE ticket = $1",
                &[&ticket],
            )
            .await?
        else {
            return Ok(None);
        };
        let events: Value = row.try_get("events")?;
        Ok(Some(
            serde_json::from_value(events).context("reading a ticket's history")?,
        ))
    }

    /// Keeps `events` as `ticket`'s issue's history, read at `read_at`.
    pub async fn put_history(
        &self,
        ticket: Uuid,
        events: &[HistoryEvent],
        read_at: DateTime<Utc>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO ticket_history (ticket, events, read_at) VALUES ($1, $2, $3) \
                 ON CONFLICT (ticket) DO UPDATE SET events = EXCLUDED.events, \
                 read_at = EXCLUDED.read_at",
                &[&ticket, &serde_json::to_value(events)?, &read_at],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "ticket_history_tests.rs"]
mod tests;
