//! `crew.*`: the overseer's tools, registered for the overseeing agent
//! alone. It reads the crew at every level; what it may do is gated by its
//! level and `[oversight.act]` (see [`super::actions`]): at `suggest`, and
//! for anything not allowed, an action becomes a suggestion the person
//! accepts or dismisses in Desk.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_desk::charge::Wheel;
use workforce_desk::crew::{self, Filter, Item, ItemState};

use super::actions::{Action, Gate};
use super::watcher::reported;
use super::CrewHub;

/// Every `crew.*` tool: the reading ones first.
pub const READ: [&str; 4] = [
    "crew.overview",
    "crew.activity",
    "crew.history",
    super::spend::NAME,
];
pub const ACT: [&str; 12] = [
    "crew.nudge",
    "crew.escalate",
    "crew.approve",
    "crew.deny",
    "crew.stop",
    "crew.interrupt",
    "crew.resume",
    "crew.restart",
    "crew.start_session",
    "crew.assign",
    "crew.take_on",
    "crew.release",
];

pub struct CrewTool {
    name: &'static str,
    crew: Weak<CrewHub>,
}

impl CrewTool {
    /// The tools an overseer at `level` has: the reading ones, and the
    /// acting ones unless it only watches.
    pub fn for_level(crew: &Arc<CrewHub>, watch_only: bool) -> Vec<Self> {
        READ.iter()
            .chain(if watch_only { [].iter() } else { ACT.iter() })
            .map(|name| Self {
                name,
                crew: Arc::downgrade(crew),
            })
            .collect()
    }
}

fn reason_schema() -> Value {
    json!({ "type": "string", "description": "Why, in a sentence the person reads in Activity (e.g. \"Quiet for 40 min while working.\")" })
}

fn session_schema() -> Value {
    json!({ "type": "string", "description": "A session id from crew.overview" })
}

fn state_word(s: ItemState) -> &'static str {
    match s {
        ItemState::NeedsYou => "needs you",
        ItemState::Working => "working",
        ItemState::Quiet => "quiet",
        ItemState::Idle => "idle",
        ItemState::Done => "done today",
        ItemState::Failed => "failed",
        ItemState::Stopped => "stopped",
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

/// Who has the wheel of each session the overseer runs, by session id.
type Charged = HashMap<String, Wheel>;

/// Whether the overseer runs it, and who drives it now.
fn charge_cell(i: &Item, charged: &Charged) -> &'static str {
    match charged.get(&i.id) {
        None => "",
        Some(Wheel::Boss) => "yes",
        Some(Wheel::Person) => "yes · the person is driving",
        Some(Wheel::PersonPinned) => "yes · the person is driving until they hand it back",
    }
}

/// One row of the overview's table.
fn row(i: &Item, charged: &Charged) -> String {
    let kind = match (i.kind, i.engine, i.origin) {
        (crew::ItemKind::Agent, ..) => "agent".to_string(),
        (_, Some(engine), Some(origin)) => format!(
            "{} session{}",
            engine.as_str(),
            if origin == workforce_desk::model::Origin::External {
                " (other terminal)"
            } else {
                ""
            }
        ),
        _ => "session".to_string(),
    };
    let mut detail = i.detail.clone().unwrap_or_default();
    if let Some(a) = &i.approval {
        detail = format!("asks: {} (approval {})", a.summary, a.id);
    }
    if let Some(q) = &i.question {
        detail = format!("asks you: {}", q.text);
    }
    // The person's name for it first; the hub's and its folder after.
    let also: Vec<String> = i
        .hub_name
        .iter()
        .cloned()
        .chain(i.cwd.as_deref().map(|c| format!("in {c}")))
        .collect();
    let who = if also.is_empty() {
        i.name.clone()
    } else {
        format!("{} ({})", i.name, also.join(", "))
    };
    format!(
        "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
        cell(&who),
        kind,
        i.id,
        state_word(i.state),
        cell(i.task.as_deref().unwrap_or("")),
        cell(&detail),
        i.since.map(|t| t.to_rfc3339()).unwrap_or_default(),
        charge_cell(i, charged),
    )
}

/// The overview as tables the overseer reads and can act from: one per
/// sidebar group, in the person's order, then whatever is in none.
fn table(items: &[Item], groups: &[String], charged: &Charged) -> String {
    const HEAD: &str = "| Who | Kind | Id | State | Doing | Detail | Since | You run it |\n|---|---|---|---|---|---|---|---|\n";
    let mut out = String::new();
    let mut section = |title: Option<&str>, rows: Vec<&Item>| {
        if rows.is_empty() {
            return;
        }
        match title {
            Some(t) => out.push_str(&format!("Group “{}”:\n", cell(t))),
            None if !out.is_empty() => out.push_str("In no group:\n"),
            None => {}
        }
        out.push_str(HEAD);
        rows.into_iter()
            .for_each(|i| out.push_str(&row(i, charged)));
        out.push('\n');
    };
    for g in groups {
        section(
            Some(g),
            items
                .iter()
                .filter(|i| i.group.as_deref() == Some(g.as_str()))
                .collect(),
        );
    }
    section(
        None,
        items
            .iter()
            .filter(|i| i.group.as_ref().is_none_or(|g| !groups.contains(g)))
            .collect(),
    );
    if out.is_empty() {
        out.push_str(HEAD);
    }
    out
}

impl CrewTool {
    async fn read(&self, crew: &Arc<CrewHub>, input: &Value) -> Result<Value, String> {
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %e, "a crew tool failed");
            "The crew could not be read just now.".to_string()
        };
        match self.name {
            "crew.overview" => {
                let o = crew
                    .desk
                    .overview(crew.as_ref(), None)
                    .await
                    .map_err(internal)?;
                let (boss, parallel) = crew
                    .overseer_service()
                    .map(|(s, o)| (s.agent_id.as_str().to_string(), o.work.parallel))
                    .unwrap_or_default();
                let charged: Charged = crew
                    .desk
                    .charges()
                    .into_iter()
                    .filter(|c| c.boss == boss)
                    .map(|c| (c.session.to_string(), c.wheel))
                    .collect();
                Ok(json!({
                    "counts": o.counts,
                    "table": reported(&table(&o.items, &o.groups, &charged), "the crew's sessions and agents"),
                    "slots": { "busy": crew.desk.charges_busy(&boss), "parallel": parallel },
                }))
            }
            "crew.activity" => {
                let limit = input["limit"].as_i64().unwrap_or(30).clamp(1, 100);
                let subject = input["subject_id"].as_str().filter(|s| !s.is_empty());
                let page = match subject {
                    Some(id) => crew
                        .desk
                        .store()
                        .events_since(
                            chrono::Utc::now() - chrono::Duration::days(7),
                            &[id.to_string()],
                            limit,
                        )
                        .await
                        .map(|mut e| {
                            e.reverse();
                            e
                        }),
                    None => crew
                        .desk
                        .activity(Filter::All, None, limit)
                        .await
                        .map(|p| p.events),
                }
                .map_err(internal)?;
                let lines: Vec<String> = page
                    .iter()
                    .map(|e| {
                        let who = &e.actor.name;
                        let about = e
                            .subject
                            .as_ref()
                            .filter(|s| s.id != e.actor.id)
                            .map(|s| format!(" (about {} {})", s.name, s.id))
                            .unwrap_or_default();
                        format!(
                            "{} {} {} {}{}{}",
                            e.at.to_rfc3339(),
                            e.kind.as_str(),
                            who,
                            e.text,
                            about,
                            e.reason
                                .as_deref()
                                .map(|r| format!(" — {r}"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                Ok(json!({ "events": reported(&lines.join("\n"), "the crew's activity") }))
            }
            "crew.history" => {
                let limit = input["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
                if let Some(session) = input["session_id"].as_str() {
                    let id: uuid::Uuid = session
                        .parse()
                        .map_err(|_| "That isn’t a session id from crew.overview.".to_string())?;
                    let v = crew
                        .desk
                        .view(id)
                        .map_err(|_| "That session isn’t there any more.".to_string())?;
                    let terminal = crew.desk.terminal_text(id, 3000).ok();
                    let said = json!({
                        "name": v.label(), "hub_name": v.name, "group": v.group.map(|g| g.name),
                        "goal": v.goal, "cwd": v.cwd,
                        "last_prompt": v.last_prompt, "last_reply": v.last_reply,
                        "pending": v.pending, "terminal": terminal,
                    });
                    return Ok(json!({
                        "state": v.state,
                        "session": reported(&said.to_string(), "the session"),
                    }));
                }
                let agent = input["agent_id"]
                    .as_str()
                    .ok_or("Say which agent or session.")?;
                let service = crew
                    .state
                    .registry
                    .get(agent)
                    .ok_or_else(|| format!("There’s no agent called {agent} on this Mac."))?;
                let Some(owner) = crew.person(Some(&service)) else {
                    return Ok(json!({ "messages": [] }));
                };
                let speaker = workforce_desk::Speaker::agent(agent, &super::name_of(&service));
                let mut page = crate::desk_conversations::page(
                    service.sessions.as_ref(),
                    &speaker,
                    &owner,
                    workforce_common::channels::DESK_CHANNEL,
                    workforce_desk::Cursor::Newest,
                    limit,
                )
                .map_err(|e| e.to_string())?;
                crate::desk_conversations::with_authors(&crew.desk, &mut page).await;
                let messages = json!(page.messages.iter().map(|m| json!({
                    "from": m.from, "author": m.author, "text": crew::clip(&m.text, 1500), "at": m.at,
                })).collect::<Vec<_>>());
                Ok(json!({
                    "messages": reported(&messages.to_string(), &format!("{agent}'s Desk DM")),
                }))
            }
            super::spend::NAME => crew.spend(input).await,
            _ => Err("not a reading tool".into()),
        }
    }

    async fn act(&self, crew: &Arc<CrewHub>, input: &Value) -> Result<Value, String> {
        let reason = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or("Give the reason: the person reads it in Activity.")?;
        let action = Action::parse(self.name, input)?;
        let (_, oversight) = crew
            .overseer_service()
            .ok_or("You don’t oversee the crew any more.")?;
        let by = crew
            .by_overseer(reason)
            .ok_or("You don’t oversee the crew any more.")?;
        crew.know_question(&action).await;
        match crew.gate(&action, &oversight) {
            Gate::Refuse => {
                Err("You only watch the crew: tell the person what you would do instead.".into())
            }
            Gate::Run => {
                let done = crew.perform(&action, by).await?;
                Ok(json!({ "done": done }))
            }
            Gate::Suggest(why) => {
                let s = crew.suggest(self.name, input, &action, &by).await?;
                // An answer it may not send is a draft the person sends.
                crew.answer_drafted(&action, &by, &why).await;
                Ok(json!({
                    "suggested": s.text,
                    "suggestion_id": s.id,
                    "note": format!("Not done: {why}. The person sees it as a suggestion beside your message and can do it or dismiss it; mention it briefly."),
                }))
            }
        }
    }
}

#[async_trait]
impl Tool for CrewTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        match self.name {
            "crew.overview" => "Everyone on the crew now: the person's agents and their Claude Code and Codex sessions, each with its state (needs you, working, quiet, done today, idle, failed, stopped), what it's doing, what it asks (with approval ids), since when, and whether you run it for the person (and whether they are driving it now). slots says how many of the sessions you run are starting or working, of the most you may run at once. For what sessions spent (tokens, cost), use crew.spend.",
            super::spend::NAME => super::spend::DESCRIPTION,
            "crew.activity" => "What happened on the crew, newest first: turns started and finished, questions, requests, failures, and what you and the person did. Filter to one agent or session with subject_id.",
            "crew.history" => "The recent messages of one agent's Desk DM with the person (agent_id), or one session's last prompt, reply, pending request and terminal (session_id).",
            "crew.nudge" => "Send a short message to an agent's DM (agent_id) or to a session on its Desk line (session_id): a check-in, a hint, a correction. A session gets it once its turn ends (crew.interrupt stops a turn now). With reply_to (a question's message id), it is your answer to what that session asked you.",
            "crew.escalate" => "Give a question a session asked you on its Desk line to the person (message_id): say why in reason, offer answers in options, and draft one in draft if you can. Use it for anything that pushes, deletes, deploys, spends, sends to people or changes scope, and whenever you are unsure.",
            "crew.approve" => "Allow a session's pending request (session_id, approval_id from crew.overview).",
            "crew.deny" => "Deny a session's pending request (session_id, approval_id from crew.overview).",
            "crew.stop" => "Stop a running Desk session; its conversation is kept. A session the person started in a Desk terminal can't be stopped (the shell is theirs): interrupt it instead.",
            "crew.interrupt" => "End the turn a working session is on, as Esc in its terminal: it, and the terminal it runs in, keep running, and the shell stays the person's. Works for sessions started in a Desk terminal too.",
            "crew.resume" => "Resume a stopped session, optionally with a prompt. A session in another terminal can only be resumed once it has ended there.",
            "crew.restart" => "Stop a session and resume it with a new prompt, e.g. to get it unstuck.",
            "crew.start_session" => "Start a Claude Code (default) or Codex session in a project folder with a goal.",
            "crew.assign" => "Hand a task to one of the crew's agents, in its DM.",
            "crew.take_on" => "Run a session for the person (session_id): it wakes you each time its turn ends, so you can tell it the next step. You take it on yourself only when the person lets you take on every session; otherwise this offers it to them. The person can take over any time by typing in it, and your messages wait until they hand it back.",
            "crew.release" => "Let a session you run go (session_id): the person runs it on their own, and its turn ends no longer wake you. Say why in reason.",
            _ => "",
        }
    }

    fn input_schema(&self) -> Value {
        let act = |mut props: serde_json::Map<String, Value>, required: &[&str]| {
            props.insert("reason".into(), reason_schema());
            let mut req: Vec<&str> = required.to_vec();
            req.push("reason");
            json!({ "type": "object", "properties": props, "required": req })
        };
        let props = |v: Value| v.as_object().cloned().unwrap_or_default();
        match self.name {
            "crew.overview" => json!({ "type": "object", "properties": {} }),
            super::spend::NAME => super::spend::schema(),
            "crew.activity" => json!({ "type": "object", "properties": {
                "subject_id": { "type": "string", "description": "An agent or session id" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            }}),
            "crew.history" => json!({ "type": "object", "properties": {
                "agent_id": { "type": "string" },
                "session_id": session_schema(),
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            }}),
            "crew.nudge" => act(
                props(json!({
                    "agent_id": { "type": "string" },
                    "session_id": session_schema(),
                    "message": { "type": "string" },
                    "reply_to": { "type": "string", "description": "The message id of the session's question this answers" }
                })),
                &["message"],
            ),
            "crew.escalate" => act(
                props(json!({
                    "message_id": { "type": "string", "description": "The message id of the session's question" },
                    "options": { "type": "array", "items": { "type": "string" }, "maxItems": 6 },
                    "draft": { "type": "string", "description": "The answer you would give, for the person to send or change" }
                })),
                &["message_id"],
            ),
            "crew.approve" | "crew.deny" => act(
                props(json!({
                    "session_id": session_schema(),
                    "approval_id": { "type": "string" }
                })),
                &["session_id", "approval_id"],
            ),
            "crew.stop" | "crew.interrupt" | "crew.take_on" | "crew.release" => act(
                props(json!({ "session_id": session_schema() })),
                &["session_id"],
            ),
            "crew.resume" | "crew.restart" => act(
                props(json!({
                    "session_id": session_schema(),
                    "prompt": { "type": "string" }
                })),
                &["session_id"],
            ),
            "crew.start_session" => act(
                props(json!({
                    "path": { "type": "string", "description": "Absolute path of the project folder" },
                    "goal": { "type": "string" },
                    "name": { "type": "string" },
                    "engine": { "type": "string", "enum": workforce_desk::engine::startable() }
                })),
                &["path", "goal"],
            ),
            "crew.assign" => act(
                props(json!({
                    "agent_id": { "type": "string" },
                    "task": { "type": "string" }
                })),
                &["agent_id", "task"],
            ),
            _ => json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let ms = || started.elapsed().as_millis() as u64;
        let Some(crew) = self.crew.upgrade() else {
            return Ok(ToolResult::failure(
                "Desk is not running.".to_string(),
                ms(),
            ));
        };
        let result = if READ.contains(&self.name) {
            self.read(&crew, &input).await
        } else {
            self.act(&crew, &input).await
        };
        Ok(match result {
            Ok(v) => ToolResult::success(v, ms()),
            Err(e) => ToolResult::failure(e, ms()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_desk::model::{Engine, Origin};

    fn item(name: &str, hub_name: Option<&str>, group: Option<&str>) -> Item {
        Item {
            kind: crew::ItemKind::Session,
            id: format!("id-{name}"),
            name: name.into(),
            hub_name: hub_name.map(Into::into),
            cwd: Some("/Users/k/dev/monorepo".into()),
            group: group.map(Into::into),
            engine: Some(Engine::Claude),
            origin: Some(Origin::Desk),
            state: ItemState::Working,
            task: None,
            detail: None,
            since: None,
            last_activity: None,
            approval: None,
            question: None,
            background: None,
        }
    }

    #[test]
    fn the_overseer_reads_sessions_under_their_groups_by_both_names() {
        let items = [
            item("auth-fix", None, None),
            item("Ledger migration", Some("monorepo-fd"), Some("Atlas")),
        ];
        let charged = Charged::from([(items[1].id.clone(), Wheel::Person)]);
        let t = table(
            &items,
            &["Atlas".to_string(), "Empty".to_string()],
            &charged,
        );
        let atlas = t.find("Group “Atlas”:").expect("a Atlas section");
        let row = t
            .find("| Ledger migration (monorepo-fd, in /Users/k/dev/monorepo) |")
            .expect("both names");
        let rest = t.find("In no group:").expect("the rest after");
        assert!(atlas < row && row < rest, "{t}");
        assert!(
            !t.contains("Empty"),
            "a group with nothing in it is left out"
        );
        assert!(t[rest..].contains("| auth-fix (in /Users/k/dev/monorepo) |"));
        assert!(t.contains("| yes · the person is driving |\n"), "{t}");
        assert!(t[rest..].trim_end().ends_with("|  |"), "{t}");
    }
}
