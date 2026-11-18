//! A board synced with GitHub, decided: a Projects v2 project (its Status
//! options are the columns, its own item order is As on GitHub) or a
//! repository's issues (open and closed, or labels the person maps). Pure,
//! like [`crate::board_sync`], whose shapes it fills: the hub's
//! `github.project_items`, `github.issues_changed` and the rest answer in
//! the shapes read here.
//!
//! An issue's key is `owner/repo#12`, lowercase, whatever board it is on.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::board::{Assignee, Facts, Filter, IssueRef, Sprint, TicketState};
use crate::board_sync::{remote_time, unfence, Issue, RemoteState};

/// The tools a GitHub board's sync needs ([`crate::board_sync::tools_of`]).
pub const TOOLS: &[&str] = workforce_core::desk::BOARD_SYNC_GITHUB_TOOLS;

/// The Projects v2 field a project board's columns are: every project has
/// it.
pub const STATUS_FIELD: &str = "Status";

/// A state that is a label (`label:in progress`), on a repository's board
/// whose person mapped a column to one.
pub const LABEL_STATE: &str = "label:";

/// Items read whole in one `github.project_items` call.
pub const IDS_PER_READ: usize = 50;

/// What a GitHub board is linked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `owner/projects/7`.
    Project { owner: String, number: u32 },
    /// `owner/repo`.
    Repo { owner: String, repo: String },
}

impl Target {
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project { .. })
    }
}

/// Whether `s` can be a GitHub account's login: letters, digits and single
/// hyphens, not at either end, at most 39.
pub fn login_ok(s: &str) -> bool {
    (1..=39).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// Whether `s` can be a repository's name there.
pub fn repo_ok(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// What a board's `target` names, when it is one.
pub fn target_of(target: &str) -> Option<Target> {
    let parts: Vec<&str> = target.split('/').collect();
    match parts.as_slice() {
        [owner, "projects", n] if login_ok(owner) => {
            let number: u32 = n.parse().ok().filter(|n| *n > 0)?;
            Some(Target::Project {
                owner: owner.to_string(),
                number,
            })
        }
        [owner, repo] if login_ok(owner) && repo_ok(repo) => Some(Target::Repo {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }),
        _ => None,
    }
}

/// Checks a GitHub board's target.
pub fn check_target(target: &str) -> Result<(), String> {
    match target_of(target) {
        Some(_) => Ok(()),
        None => Err(format!(
            "{target:?} isn't a GitHub project (owner/projects/7) or repository (owner/repo)."
        )),
    }
}

/// Checks a project's or repository's `owner/repo` for new tickets.
pub fn check_repo(repo: &str) -> Result<(), String> {
    match target_of(repo) {
        Some(Target::Repo { .. }) => Ok(()),
        _ => Err(format!("{repo:?} isn't a GitHub repository (owner/repo).")),
    }
}

/// An issue's key, `owner/repo#12`, from one as typed (any case); none
/// when it isn't one.
pub fn parse_key(s: &str) -> Option<String> {
    let (owner, repo, n) = key_parts(s.trim())?;
    Some(format!("{owner}/{repo}#{n}").to_ascii_lowercase())
}

/// A key's owner, repository and number.
pub fn key_parts(key: &str) -> Option<(&str, &str, u64)> {
    let (repo_path, n) = key.rsplit_once('#')?;
    let (owner, repo) = repo_path.split_once('/')?;
    let n: u64 = n.parse().ok().filter(|n| *n > 0)?;
    (login_ok(owner) && repo_ok(repo)).then_some((owner, repo, n))
}

/// The key of issue `number` in `repo` (`owner/repo`).
pub fn key_of(repo: &str, number: u64) -> Option<String> {
    parse_key(&format!("{repo}#{number}"))
}

/// Whether the issue `key` belongs on a board linked to `target`: any issue
/// on a project (its listing says which are), only the repository's own on
/// a repository's board.
pub fn key_in_target(key: &str, target: &str) -> bool {
    let Some(key) = parse_key(key) else {
        return false;
    };
    match target_of(target) {
        Some(Target::Project { .. }) => true,
        Some(Target::Repo { owner, repo }) => key
            .strip_prefix(&format!("{owner}/{repo}#").to_ascii_lowercase())
            .is_some(),
        None => false,
    }
}

/// Whether `id` can be the person's id there: a login.
pub fn check_me(id: &str) -> Result<(), String> {
    match login_ok(id) {
        true => Ok(()),
        false => Err(format!("{id:?} isn't a GitHub login.")),
    }
}

// ── Filters ──────────────────────────────────────────────────────────────

/// Checks a raw project filter (`label:bug -status:Done`): one line, quotes
/// that close, short.
pub fn check_query(q: &str) -> Result<(), String> {
    let q = q.trim();
    if q.is_empty() || q.len() > 500 || q.contains(['\n', '\r']) {
        return Err("Type the project filter to import by, on one line.".into());
    }
    if q.chars().filter(|c| *c == '"').count() % 2 == 1 {
        return Err("The filter's quotes don't match.".into());
    }
    Ok(())
}

/// A word as a project filter's value: quoted, without quotes of its own.
fn quoted(v: &str) -> String {
    format!("\"{}\"", v.trim().replace('"', ""))
}

/// The `query` a project's listing takes for `f`; none lists every item.
/// `me` is the person's login.
pub fn project_query(f: &Filter, me: Option<&str>) -> Option<String> {
    match f {
        // What is closed comes on only with the recently done: without it
        // the listing is the open items, however many were ever closed.
        Filter::All {
            include_done: false,
        } => Some("is:open".into()),
        Filter::All { .. } | Filter::CurrentCycle => None,
        Filter::AssignedToMe => me.filter(|m| login_ok(m)).map(|m| format!("assignee:{m}")),
        Filter::Label(l) => Some(format!("label:{}", quoted(l))),
        Filter::Milestone(m) => Some(format!("milestone:{}", quoted(m))),
        Filter::Query(q) => Some(q.trim().to_string()),
    }
}

/// The `github.issues_changed` arguments a repository board's filter
/// means, from `since` (the cursor) and at `page`.
pub fn repo_args(
    owner: &str,
    repo: &str,
    f: &Filter,
    me: Option<&str>,
    since: Option<&str>,
    page: u32,
) -> Value {
    let mut args = json!({
        "owner": owner,
        "repo": repo,
        "state": "all",
        "page": page.max(1),
        "per_page": crate::board_sync::PAGE,
    });
    if let Some(since) = since {
        args["since"] = json!(since);
    }
    match f {
        // The open; a board that takes the recently done lists those too,
        // on their own (the desk's `repo_matching`).
        Filter::All { .. } if since.is_none() => args["state"] = json!("open"),
        Filter::AssignedToMe => {
            if let Some(m) = me.filter(|m| login_ok(m)) {
                args["assignee"] = json!(m);
            }
        }
        Filter::Label(l) => args["labels"] = json!(l.trim()),
        Filter::Milestone(m) => args["milestone"] = json!(m.trim()),
        _ => {}
    }
    args
}

// ── Issues ───────────────────────────────────────────────────────────────

/// One item of a project's lean listing: its id, its issue's key, and when
/// it or its issue last changed. Drafts, pull requests and items the key
/// can't see aren't issues, and aren't kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Lean {
    pub item_id: String,
    pub key: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A `github.project_items` page: its issues' items, in the project's
/// order, and the page after it.
pub fn lean_items(answer: &Value) -> (Vec<Lean>, Option<String>, bool) {
    let mut lossy = false;
    let items = answer["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            match i["type"].as_str() {
                Some("ISSUE") => {}
                // An item the key can't see (a repository it lost) is still
                // on the project: the listing can't say what left it.
                Some("REDACTED") | None => {
                    lossy = true;
                    return None;
                }
                // Pull requests and drafts aren't a board's tickets.
                Some(_) => return None,
            }
            let lean = (|| {
                Some(Lean {
                    item_id: id_of(&i["item_id"])?,
                    key: key_of(i["repo"].as_str()?, i["number"].as_u64()?)?,
                    updated_at: remote_time(&i["updated_at"])?,
                })
            })();
            lossy |= lean.is_none();
            lean
        })
        .collect();
    let next = match answer["page_info"]["has_next_page"].as_bool() {
        Some(true) => {
            let next = answer["page_info"]["end_cursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            // More to come, but no way on to it: not the whole listing.
            lossy |= next.is_none();
            next
        }
        _ => None,
    };
    (items, next, lossy)
}

/// A node id as GitHub gives one: short, printable, no spaces.
fn id_of(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| (1..=100).contains(&s.len()))
        .filter(|s| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '='))
        })
        .map(str::to_string)
}

/// Whether `id` can be a GitHub node's id (a project's, an item's).
pub fn check_node_id(id: &str) -> Result<(), String> {
    match id_of(&json!(id)) {
        Some(_) => Ok(()),
        None => Err(format!("{id:?} isn't a GitHub id.")),
    }
}

/// A name someone else wrote, on one line and not too long.
fn name_of(v: &Value) -> Option<String> {
    let n = unfence(v.as_str()?).0;
    let n = crate::crew::one_line(n.trim(), 80);
    (!n.is_empty()).then_some(n)
}

/// The issue in a `github.project_items` full item or a
/// `github.issues_changed` row, with its state as the board's `states`
/// read it, and its place there when the project gives one. None for what
/// isn't an issue.
pub fn issue_of(
    v: &Value,
    states: &BTreeMap<TicketState, String>,
    project: bool,
    rank: Option<String>,
) -> Option<Issue> {
    if v.get("type").is_some_and(|t| t.as_str() != Some("ISSUE")) {
        return None;
    }
    let repo = v["repo"].as_str()?;
    let key = key_of(repo, v["number"].as_u64()?)?;
    let (title, f1) = match v["title"].as_str() {
        Some(t) => unfence(t),
        None => (String::new(), false),
    };
    let (body, f2) = match v["body"].as_str() {
        Some(b) => unfence(b),
        None => (String::new(), false),
    };
    let (body, clipped) = crate::board_sync::held_body(body);
    let labels: Vec<String> = v["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(name_of)
        .take(20)
        .collect();
    let closed = v["state"]
        .as_str()
        .map(|s| s.eq_ignore_ascii_case("closed"));
    let not_planned = v["state_reason"]
        .as_str()
        .is_some_and(|r| r.eq_ignore_ascii_case("not_planned"));
    let state_kind = match closed {
        Some(true) if not_planned => Some("canceled".to_string()),
        Some(true) => Some("completed".to_string()),
        _ => None,
    };
    let state = match project {
        true => {
            let status = name_of(&v["status"]).unwrap_or_default();
            // A closed issue is done or dropped whatever its Status says (a
            // project with no Dropped status keeps its old one): its column
            // is the one closing it means.
            match &state_kind {
                Some(kind) => {
                    let shut = match kind.as_str() {
                        "canceled" => TicketState::Dropped,
                        _ => TicketState::Done,
                    };
                    match crate::board_sync::column_for(states, &status, Some(kind)) {
                        Some(TicketState::Done | TicketState::Dropped) => status,
                        _ => states.get(&shut).cloned().unwrap_or_else(|| match shut {
                            TicketState::Dropped => "Dropped".into(),
                            _ => "Done".into(),
                        }),
                    }
                }
                None => status,
            }
        }
        false => repo_state(closed == Some(true), &labels, states),
    };
    let assignee = v.get("assignees").map(|a| {
        a.as_array().and_then(|a| a.first()).and_then(|u| {
            let login = u["login"].as_str().filter(|l| login_ok(l))?;
            Some(Assignee {
                id: login.to_string(),
                name: name_of(&u["name"]).unwrap_or_else(|| login.to_string()),
            })
        })
    });
    let mut facts = Facts {
        kind: name_of(&v["issue_type"]),
        remote_state: Some(state.clone()).filter(|s| !s.is_empty()),
        state_kind: state_kind.clone(),
        reporter: name_of(&v["author"]),
        created: remote_time(&v["created_at"]),
        ..Facts::default()
    };
    if let Some(m) = v.get("milestone").filter(|m| m.is_object()) {
        facts.sprint = Some(Sprint {
            name: name_of(&m["title"]),
            number: m["number"].as_u64().and_then(|n| u32::try_from(n).ok()),
            starts: None,
            ends: m["due_on"]
                .as_str()
                .and_then(|d| chrono::NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok()),
        });
    }
    if let Some(p) = v.get("parent").filter(|p| p.is_object()) {
        if let Some(pk) = p["repo"]
            .as_str()
            .zip(p["number"].as_u64())
            .and_then(|(r, n)| key_of(r, n))
        {
            facts.parent = Some(IssueRef {
                key: pk,
                title: name_of(&p["title"]).unwrap_or_default(),
                ..IssueRef::default()
            });
        }
    }
    Some(Issue {
        key,
        id: if project { id_of(&v["item_id"]) } else { None },
        url: v["url"]
            .as_str()
            .filter(|u| crate::board_sync::safe_href(u, true))
            .unwrap_or_default()
            .to_string(),
        title,
        body: v.get("body").map(|_| body),
        state,
        state_kind,
        updated_at: remote_time(&v["updated_at"])?,
        labels,
        blocked_by: None,
        flagged: f1 || f2,
        assignee,
        closed,
        clipped,
        priority: None,
        project: None,
        due: None,
        files: None,
        facts,
        children_partial: false,
        upstream_rank: rank,
    })
}

/// A repository issue's state as the board reads it: closed, else the
/// first column's label it carries (in the board's own column order), else
/// open.
fn repo_state(closed: bool, labels: &[String], states: &BTreeMap<TicketState, String>) -> String {
    if closed {
        return "Closed".into();
    }
    states
        .values()
        .filter_map(|s| s.strip_prefix(LABEL_STATE))
        .find(|l| labels.iter().any(|x| x.eq_ignore_ascii_case(l)))
        .map(|l| format!("{LABEL_STATE}{l}"))
        .unwrap_or_else(|| "Open".into())
}

/// The states a project's columns can be: its Status options, by name, as
/// `github.project_get` answered.
pub fn project_states(answer: &Value) -> Vec<RemoteState> {
    answer["status"]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| name_of(&o["name"]))
        .map(|name| RemoteState {
            kind: kind_of_status(&name),
            name,
        })
        .collect()
}

/// A Status option's type, by its words: what GitHub's default options mean.
fn kind_of_status(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let kind = match n.as_str() {
        "todo" | "to do" | "ready" => "unstarted",
        "in progress" => "started",
        "done" => "completed",
        _ => return None,
    };
    Some(kind.to_string())
}

/// The states a repository's columns can be: open, closed, and each label
/// ([`LABEL_STATE`]) `github.repo_labels` answered.
pub fn repo_states(answer: &Value) -> Vec<RemoteState> {
    let mut out = vec![
        RemoteState {
            name: "Open".into(),
            kind: Some("unstarted".into()),
        },
        RemoteState {
            name: "Closed".into(),
            kind: Some("completed".into()),
        },
    ];
    out.extend(
        answer["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| name_of(&l["name"]))
            .map(|n| RemoteState {
                name: format!("{LABEL_STATE}{n}"),
                kind: None,
            }),
    );
    out
}

// ── What a push sends ────────────────────────────────────────────────────

/// The calls that move issue `key` to the remote state `to` from `from`
/// (what its column there was), on a project or a repository. Done and
/// Dropped close it there (not planned for Dropped); any other column
/// reopens one that was closed.
pub fn move_calls(
    s: &crate::board::Sync,
    key: &str,
    item: Option<&str>,
    column: TicketState,
    to: Option<&str>,
    from: Option<&str>,
    was_closed: bool,
) -> Vec<(&'static str, Value)> {
    let Some((owner, repo, number)) = key_parts(key) else {
        return vec![];
    };
    let issue = json!({ "owner": owner, "repo": repo, "number": number });
    let mut calls = vec![];
    let project = target_of(&s.target).is_some_and(|t| t.is_project());
    // Closed or reopened first: GitHub's own project workflows set Status
    // on a close or a reopen, and the column's own Status, after, wins.
    let closes = matches!(column, TicketState::Done | TicketState::Dropped)
        || (!project && to.is_some_and(|t| t.eq_ignore_ascii_case("closed")));
    if closes {
        let mut args = issue.clone();
        args["state"] = json!("closed");
        args["state_reason"] = json!(match column {
            TicketState::Dropped => "not_planned",
            _ => "completed",
        });
        calls.push(("github.issue_update", args));
    } else if was_closed {
        let mut args = issue.clone();
        args["state"] = json!("open");
        args["state_reason"] = json!("reopened");
        calls.push(("github.issue_update", args));
    }
    // A column mapped to nothing there only closes or reopens.
    let Some(to) = to else {
        return calls;
    };
    if project {
        if let (Some(project_id), Some(item)) = (s.target_id.as_deref(), item) {
            calls.push((
                "github.project_item_status",
                json!({ "project_id": project_id, "item_id": item, "status": to, "field": STATUS_FIELD }),
            ));
        }
    } else {
        let add: Vec<&str> = to.strip_prefix(LABEL_STATE).into_iter().collect();
        let remove: Vec<&str> = from
            .and_then(|f| f.strip_prefix(LABEL_STATE))
            .filter(|f| !add.iter().any(|a| a.eq_ignore_ascii_case(f)))
            .into_iter()
            .collect();
        if !add.is_empty() || !remove.is_empty() {
            let mut args = issue.clone();
            args["add"] = json!(add);
            args["remove"] = json!(remove);
            calls.push(("github.issue_labels", args));
        }
    }
    calls
}

/// The `github.project_item_move` call a drop sends, when it says where.
pub fn order_call(
    project_id: Option<&str>,
    item: Option<&str>,
    mv: &crate::board::RankMove,
) -> Option<Value> {
    let (project_id, item) = (project_id?, item?);
    match (&mv.after_item, mv.to_top) {
        (Some(after), _) => {
            Some(json!({ "project_id": project_id, "item_id": item, "after_id": after }))
        }
        (None, true) => {
            Some(json!({ "project_id": project_id, "item_id": item, "after_id": null }))
        }
        (None, false) => None,
    }
}

/// The arguments that read issue `key` whole, by its own repository.
pub fn read_args(key: &str) -> Value {
    match key_parts(key) {
        Some((owner, repo, number)) => json!({ "owner": owner, "repo": repo, "number": number }),
        None => json!({}),
    }
}

/// Who the person is, as `github.viewer` answered: them only on their own
/// key.
pub fn viewer_of(answer: &Value) -> crate::board_sync::Who {
    use crate::board_sync::{NoMatch, Who};
    match answer.get("personal").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return Who::Shared(NoMatch::EmailHidden),
        None => return Who::Unsaid,
    }
    let Some(login) = answer["login"].as_str().filter(|l| login_ok(l)) else {
        return Who::Unsaid;
    };
    Who::Person(Assignee {
        id: login.to_string(),
        name: name_of(&answer["name"]).unwrap_or_else(|| login.to_string()),
    })
}

#[cfg(test)]
#[path = "github_sync_tests.rs"]
mod tests;
