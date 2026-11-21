//! A session's Linear and Jira, through the boards the person linked: the
//! line's `linear` and `jira` tools, decided. Each call is checked here,
//! goes through the link whose project or team it names, as the person who
//! linked it, and every write it makes is signed as Desk on their behalf.
//! What an issue says comes back fenced as its writers' words. Where the
//! link's credentials are is the hub's ([`Issues`]): on the person's own
//! computer it runs the call; on a machine it hands it to Desk there.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::board::{Sync, System};
use crate::board_sync::{self as sync, Moved, Remote, RemoteError};
use crate::model::text_enum;

text_enum!(Action {
    List => "list",
    Search => "search",
    Get => "get",
    Comment => "comment",
    States => "states",
    Move => "move",
});

impl Action {
    /// Whether it changes the issue there.
    pub fn writes(self) -> bool {
        matches!(self, Self::Comment | Self::Move)
    }
}

/// How long a session waits for its call, wherever it runs: within the
/// plugin's step.
pub const WAIT: Duration = Duration::from_secs(20);
/// How long the computer holding the link gives the call itself.
pub const RUN_FOR: Duration = Duration::from_secs(15);
/// How many issues a list answers when it doesn't say, and at most.
pub const LISTED: usize = 20;
pub const MOST_LISTED: usize = 50;
/// The longest comment a session posts.
pub const MOST_COMMENT: usize = 20_000;
/// The most of an answer a session reads, before it is fenced.
pub const MOST_ANSWER: usize = 60_000;

/// A session's call as it asked it, for `system`: what a machine hands on,
/// and the computer holding the link checks again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    pub system: System,
    pub input: Value,
}

impl Asked {
    /// The action it names, when it names one.
    pub fn action(&self) -> Option<Action> {
        self.input["action"]
            .as_str()
            .and_then(|a| Action::parse(a).ok())
    }
}

/// The session a call is made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker {
    pub session: Uuid,
    pub name: String,
}

/// Where a session's calls go: the hub's.
#[async_trait]
pub trait Issues: Send + std::marker::Sync {
    /// `asked` run for `asker`: what it answers, or its words for why not.
    async fn issue(&self, asker: &Asker, asked: &Asked) -> Result<Value, String>;
}

/// A call checked: everything it may say, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueCall {
    pub system: System,
    pub action: Action,
    pub key: Option<String>,
    pub team: Option<String>,
    pub query: Option<String>,
    pub text: Option<String>,
    pub state: Option<String>,
    pub mine: bool,
    pub limit: usize,
}

impl IssueCall {
    /// The project or team it is about: the key's, else the one it names.
    pub fn target(&self) -> Option<&str> {
        self.key
            .as_deref()
            .and_then(|k| k.rsplit_once('-'))
            .map(|(t, _)| t)
            .or(self.team.as_deref())
    }
}

/// `input` checked as a call of `system`'s tool.
pub fn call_of(system: System, input: &Value) -> Result<IssueCall, String> {
    if system == System::Github {
        return Err("Desk's line has no GitHub issue tool.".into());
    }
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let action = s("action")
        .ok_or_else(|| "Say an action: list, search, get, comment, states or move.".to_string())
        .and_then(|a| {
            Action::parse(&a).map_err(|_| {
                format!("action is list, search, get, comment, states or move, not {a:?}.")
            })
        })?;
    let key = s("key")
        .map(|k| {
            sync::parse_issue_key(&k).ok_or_else(|| {
                format!("{k:?} isn't a {} issue's key, like VEL-15.", system.title())
            })
        })
        .transpose()?;
    let team = s("team")
        .map(|t| t.to_ascii_uppercase())
        .map(|t| sync::check_target(system, &t).map(|()| t))
        .transpose()?;
    let call = IssueCall {
        system,
        action,
        key,
        team,
        query: s("query"),
        text: input
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        state: s("state"),
        mine: input.get("mine").and_then(Value::as_bool).unwrap_or(false),
        limit: input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(LISTED, |l| {
                usize::try_from(l)
                    .unwrap_or(MOST_LISTED)
                    .clamp(1, MOST_LISTED)
            }),
    };
    let needs_key = match (action, system) {
        (Action::Get | Action::Comment | Action::Move, _) => true,
        (Action::States, System::Jira) => true,
        (Action::States, _) => call.team.is_none(),
        _ => false,
    };
    if needs_key && call.key.is_none() {
        return Err(format!(
            "Say which issue, by its key: {{\"action\": \"{}\", \"key\": \"VEL-15\"}}.",
            action.as_str()
        ));
    }
    match action {
        Action::Search if call.query.is_none() => Err("Say what to search for, in query.".into()),
        Action::Comment => match &call.text {
            None => Err("Say the comment, in text.".into()),
            Some(t) if t.chars().count() > MOST_COMMENT => Err(format!(
                "Keep a comment to at most {MOST_COMMENT} characters."
            )),
            Some(_) => Ok(call),
        },
        Action::Move if call.state.is_none() => Err(format!(
            "Say which state, by its name in {} (states lists them).",
            system.title()
        )),
        _ => Ok(call),
    }
}

/// The input's checked call, kept as asked.
pub fn asked(system: System, input: &Value) -> Result<Asked, String> {
    call_of(system, input).map(|_| Asked {
        system,
        input: input.clone(),
    })
}

/// Of `links`, the one `call` goes through: the one linked to its project
/// or team, else the first of its system.
pub fn pick<'a>(links: &'a [Sync], call: &IssueCall) -> Option<&'a Sync> {
    let ours = || links.iter().filter(|s| s.system == call.system);
    call.target()
        .and_then(|t| ours().find(|s| s.target == t))
        .or_else(|| ours().next())
}

/// Who a write is made by: Desk, for the person, from a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    pub person: String,
    pub session: String,
    /// The machine the session runs on; none: the person's own computer.
    pub machine: Option<String>,
}

impl By {
    /// What a write is signed with.
    pub fn mark(&self) -> String {
        let one = |s: &str| crate::crew::one_line(s, 80);
        let place = match &self.machine {
            Some(m) => format!(" on {}", one(m)),
            None => String::new(),
        };
        format!(
            "Desk on behalf of {} (the {} session{place})",
            one(&self.person),
            one(&self.session)
        )
    }

    /// `said`, signed.
    pub fn signed(&self, said: &str) -> String {
        format!("{}:\n\n{said}", self.mark())
    }
}

/// The words a session reads when its call got no answer in time.
pub fn no_answer(asked: &Asked) -> String {
    let system = asked.system.title();
    match asked.action().is_some_and(Action::writes) {
        true => format!(
            "{system} didn't answer in time, so this may have gone through. Read the issue (get) before you try again."
        ),
        false => format!("{system} didn't answer in time; try again."),
    }
}

/// `answer` as a session reads it: fenced as the issue's writers' words,
/// clipped at [`MOST_ANSWER`].
fn read(system: System, of: &str, answer: &Value) -> Value {
    let text = serde_json::to_string_pretty(answer).unwrap_or_default();
    let text = match text.char_indices().nth(MOST_ANSWER) {
        Some((at, _)) => format!(
            "{}\n… (clipped: ask for fewer, or for one issue)",
            &text[..at]
        ),
        None => text,
    };
    Value::String(sync::fence(system, of, &text, &sync::nonce()))
}

/// What a list of `team`'s issues is fenced as.
fn listed(team: &str) -> String {
    format!("list ({team})")
}

/// The Linear team `team`'s id: `link`'s own, else looked up by its key.
async fn team_id(remote: &dyn Remote, link: &Sync, team: &str) -> Result<String, RemoteError> {
    if let Some(id) = link.target_id.as_ref().filter(|_| link.target == team) {
        return Ok(id.clone());
    }
    let teams = sync::call(remote, System::Linear, "linear.teams_list", json!({})).await?;
    teams["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["key"].as_str() == Some(team))
        .and_then(|t| t["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| RemoteError::Failed(format!("Linear has no team {team} you can see.")))
}

/// The JQL for `call`'s list or search, newest change first.
fn jql(link: &Sync, call: &IssueCall) -> String {
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let clauses: Vec<String> = match call.action {
        Action::Search => call
            .team
            .iter()
            .map(|t| format!("project = {}", quoted(t)))
            .chain(call.query.iter().map(|q| format!("text ~ {}", quoted(q))))
            .collect(),
        _ => {
            let project = call.team.as_deref().unwrap_or(&link.target);
            [
                Some(format!("project = {}", quoted(project))),
                Some("statusCategory != Done".to_string()),
                call.mine
                    .then(|| sync::assigned_to_me(sync::shared_me(link))),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
    };
    format!("{} ORDER BY updated DESC", clauses.join(" AND "))
}

/// `call` run through `link` with `remote`, its writes signed by `by`.
pub async fn run(
    remote: &dyn Remote,
    link: &Sync,
    call: &IssueCall,
    by: &By,
) -> Result<Value, RemoteError> {
    let system = call.system;
    let key = call.key.as_deref().unwrap_or_default();
    let jira = system == System::Jira;
    let get = |tool: &'static str, args: Value| sync::call(remote, system, tool, args);
    match call.action {
        Action::List if call.mine && link.me.is_none() => Err(RemoteError::Failed(format!(
            "Desk doesn't know who you are on {} yet; list without mine.",
            system.title()
        ))),
        Action::List | Action::Search if jira => {
            let answer = get(
                "jira.search",
                json!({ "jql": jql(link, call), "max_results": call.limit }),
            )
            .await?;
            Ok(
                json!({ "issues": read(system, &listed(call.target().unwrap_or(&link.target)), &answer) }),
            )
        }
        Action::List => {
            let team = call.team.as_deref().unwrap_or(&link.target);
            let mut args = json!({
                "team_id": team_id(remote, link, team).await?,
                "open_only": true,
                "limit": call.limit,
            });
            if call.mine {
                args["assignee_id"] = json!(link.me);
            }
            let answer = get("linear.issue_list", args).await?;
            Ok(json!({ "issues": read(system, &listed(team), &answer) }))
        }
        Action::Search => {
            let query = call.query.as_deref().unwrap_or_default();
            let answer = get(
                "linear.issue_search",
                json!({ "query": query, "limit": call.limit }),
            )
            .await?;
            Ok(json!({ "issues": read(system, "search results", &answer) }))
        }
        Action::Get => {
            let (issue, comments) = match jira {
                true => (
                    get("jira.get_issue", json!({ "issue_key": key })).await?,
                    get("jira.list_comments", json!({ "issue_key": key })).await,
                ),
                false => (
                    get("linear.issue_get", json!({ "identifier": key })).await?,
                    get("linear.comment_list", json!({ "identifier": key })).await,
                ),
            };
            let mut out = json!({ "issue": read(system, key, &issue) });
            match comments {
                Ok(c) => out["comments"] = read(system, key, &c),
                Err(e) => out["comments_problem"] = json!(e.to_string()),
            }
            Ok(out)
        }
        Action::Comment => {
            let body = by.signed(call.text.as_deref().unwrap_or_default());
            match jira {
                true => get(
                    "jira.add_comment",
                    json!({ "issue_key": key, "body": body }),
                ),
                false => get(
                    "linear.comment_add",
                    json!({ "identifier": key, "body": body }),
                ),
            }
            .await?;
            Ok(json!({ "done": format!("Commented on {key}, signed as {}.", by.mark()) }))
        }
        Action::States => {
            let answer = match jira {
                true => get("jira.list_transitions", json!({ "issue_key": key })).await?,
                false => {
                    let team = call.target().unwrap_or(&link.target);
                    let id = team_id(remote, link, team).await?;
                    get("linear.states_list", json!({ "team_id": id })).await?
                }
            };
            Ok(
                json!({ "states": read(system, &format!("states ({})", call.target().unwrap_or(key)), &answer) }),
            )
        }
        Action::Move => {
            let state = call.state.as_deref().unwrap_or_default();
            let team = match jira {
                true => String::new(),
                false => team_id(remote, link, call.target().unwrap_or(&link.target)).await?,
            };
            let to = match sync::move_issue(remote, system, &team, key, state, || Ok(())).await? {
                Moved::To(to) => to,
                Moved::NoWay(ways) => {
                    return Err(RemoteError::Failed(format!(
                        "{key} can't move to {state:?} from where it is. It can move to {}.",
                        match ways.is_empty() {
                            true => "no other state".to_string(),
                            false => sync::or_list(&ways),
                        }
                    )))
                }
            };
            let note = by.signed(&format!("moved this to {to}."));
            let noted = match jira {
                true => get(
                    "jira.add_comment",
                    json!({ "issue_key": key, "body": note }),
                ),
                false => get(
                    "linear.comment_add",
                    json!({ "identifier": key, "body": note }),
                ),
            }
            .await;
            Ok(json!({ "done": match noted {
                Ok(_) => format!("Moved {key} to {to}, with a note signed as {}.", by.mark()),
                Err(e) => format!("Moved {key} to {to}; its note saying so couldn't be posted ({e})."),
            } }))
        }
    }
}

/// The input schema of `system`'s tool.
pub fn schema(system: System) -> Value {
    let (team, key) = match system {
        System::Jira => ("project", "ENG-123"),
        _ => ("team", "VEL-15"),
    };
    json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["list", "search", "get", "comment", "states", "move"] },
            "key": { "type": "string", "description": format!("The issue's key, like {key}: for get, comment, move, and states") },
            "team": { "type": "string", "description": format!("For list, search and states: the {team}'s key; by default the linked board's") },
            "query": { "type": "string", "description": "For search: the words to look for" },
            "text": { "type": "string", "maxLength": MOST_COMMENT, "description": "For comment: what to say, as markdown" },
            "state": { "type": "string", "description": "For move: the state to move it to, by its name there (states lists them)" },
            "mine": { "type": "boolean", "description": "For list: only issues assigned to the person" },
            "limit": { "type": "integer", "minimum": 1, "maximum": MOST_LISTED, "description": "For list and search: how many at most (default 20)" }
        },
        "required": ["action"]
    })
}

#[cfg(test)]
#[path = "issues_tests.rs"]
mod tests;
