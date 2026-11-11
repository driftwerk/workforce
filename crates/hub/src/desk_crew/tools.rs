//! `crew.*`: the overseer's tools, registered for each overseeing agent
//! alone, bound to the scope it oversees. It reads its scope at every
//! level, and nothing outside it: every read is filtered by what its scope
//! sees, and an action on an id outside it is refused before the gate.
//! What it may do inside is gated by its level and `[oversight.act]` (see
//! [`super::actions`]): at `suggest`, and for anything not allowed, an
//! action becomes a suggestion the person accepts or dismisses in Desk.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::oversight::{Oversight, Scope};
use workforce_desk::charge::Wheel;
use workforce_desk::crew::{self, Item, ItemState};
use workforce_desk::scope::Seen;

use crate::serve::AgentService;

use super::actions::{Action, Gate};
use super::watcher::reported;
use super::CrewHub;

/// Every `crew.*` tool: the reading ones first.
pub const READ: [&str; 5] = [
    "crew.overview",
    "crew.activity",
    "crew.history",
    super::spend::NAME,
    super::board::NAME,
];
pub const ACT: [&str; 18] = [
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
    "crew.assign_ticket",
    "crew.move_ticket",
    "crew.take_on",
    "crew.release",
    super::plan::NAME,
    super::plan::UPDATE,
    super::handover::NAME,
    "crew.handoff",
];

pub struct CrewTool {
    name: &'static str,
    crew: Weak<CrewHub>,
    /// The overseer it is registered for.
    agent: String,
    /// The scope that overseer oversees: all it reads and acts on.
    scope: Scope,
}

impl CrewTool {
    /// The tools the overseer `agent` of `scope` has at its level: the
    /// reading ones, and the acting ones unless it only watches.
    pub fn for_level(
        crew: &Arc<CrewHub>,
        agent: &str,
        scope: &Scope,
        watch_only: bool,
    ) -> Vec<Self> {
        READ.iter()
            .chain(if watch_only { [].iter() } else { ACT.iter() })
            .map(|name| Self {
                name,
                crew: Arc::downgrade(crew),
                agent: agent.to_string(),
                scope: scope.clone(),
            })
            .collect()
    }

    /// The scope it is bound to.
    #[cfg(test)]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// The overseer a tool runs for, and what its scope sees now.
pub(super) struct Bound {
    pub service: Arc<AgentService>,
    pub oversight: Oversight,
    pub seen: Seen,
}

fn reason_schema() -> Value {
    json!({ "type": "string", "description": "Why, in a sentence the person reads in Activity (e.g. \"Quiet for 40 min while working.\")" })
}

fn session_schema() -> Value {
    json!({ "type": "string", "description": "A session id from crew.overview" })
}

fn ticket_schema() -> Value {
    json!({ "type": "string", "description": "A ticket's key from crew.board, like ACM-12" })
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

pub(super) fn cell(s: &str) -> String {
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
    async fn read(&self, crew: &Arc<CrewHub>, b: &Bound, input: &Value) -> Result<Value, String> {
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %e, "a crew tool failed");
            "The crew could not be read just now.".to_string()
        };
        let seen = &b.seen;
        match self.name {
            "crew.overview" => {
                let o = crew
                    .desk
                    .overview_for(crew.as_ref(), None, Some(seen))
                    .await
                    .map_err(internal)?;
                let (boss, parallel) = (
                    b.service.agent_id.as_str().to_string(),
                    b.oversight.work.parallel,
                );
                let charged: Charged = crew
                    .desk
                    .charges()
                    .into_iter()
                    .filter(|c| c.boss == boss)
                    .map(|c| (c.session.to_string(), c.wheel))
                    .collect();
                let local = json!({
                    "counts": o.counts,
                    "table": reported(&table(&o.items, &o.groups, &charged), "the crew's sessions and agents"),
                    "slots": { "busy": crew.desk.charges_busy(&boss), "parallel": parallel },
                    "plans": crew.plans_seen(seen).await,
                });
                Ok(self.with_machines(crew, b, &boss, local).await)
            }
            "crew.activity" => {
                let limit = input["limit"].as_i64().unwrap_or(30).clamp(1, 100);
                let subject = input["subject_id"].as_str().filter(|s| !s.is_empty());
                if let Some(id) = subject {
                    crew.subject_seen(id, seen)?;
                }
                let since = match subject {
                    Some(_) => chrono::Utc::now() - chrono::Duration::days(7),
                    None => chrono::Utc::now() - chrono::Duration::days(30),
                };
                let ids: Vec<String> = subject.map(str::to_string).into_iter().collect();
                let page = crew
                    .desk
                    .store()
                    .events_since(since, &ids, limit, Some(seen))
                    .await
                    .map(|mut e| {
                        e.reverse();
                        e
                    })
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
                    crew.session_seen(id, seen)?;
                    let v = crew.desk.view(id).map_err(|_| crew.not_ours(&self.scope))?;
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
                let service = crew.agent_seen(agent, seen)?;
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
            super::spend::NAME => crew.spend(input, seen).await,
            super::board::NAME => crew.board(seen).await,
            _ => Err("not a reading tool".into()),
        }
    }

    async fn act(&self, crew: &Arc<CrewHub>, b: &Bound, input: &Value) -> Result<Value, String> {
        let reason = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or("Give the reason: the person reads it in Activity.")?;
        // Only the id Desk writes below is ever trusted: one the model
        // brings is dropped, so the key it names is the ticket acted on.
        let mut kept = input.clone();
        if let Some(o) = kept.as_object_mut() {
            o.remove("ticket_id");
        }
        let mut action = Action::parse(self.name, &kept)?;
        // Nothing outside its scope is looked into, before anything is said
        // of it or the gate weighs it.
        crew.in_scope(&action, &b.seen).await?;
        // Handing work to another engine is suggested only where the
        // person switched it on.
        if matches!(action, Action::Handoff { .. }) && !b.oversight.act.hand_off {
            return Err("The person hasn't let you suggest continuing sessions in another engine; tell them what you would suggest instead.".into());
        }
        // A ticket is read before the gate looks at what is said of it, and
        // a suggestion keeps its id and its key, so it is that ticket that
        // is shown and acted on. One on another scope's board is refused
        // as one that isn't there.
        crew.resolve_ticket(
            &mut action,
            Some(&b.seen),
            Some(b.service.agent_id.as_str()),
        )
        .await?;
        if let (Some(id), Some(key), Some(o)) = (
            action.ticket_id(),
            action.ticket_key(),
            kept.as_object_mut(),
        ) {
            o.insert("ticket_id".into(), json!(id.to_string()));
            o.insert("ticket".into(), json!(key));
        }
        let input = &kept;
        let oversight = &b.oversight;
        let by = crew.by_boss(&b.service, reason);
        crew.know_question(&action).await;
        match crew.gate(&action, oversight) {
            Gate::Refuse => {
                Err("You only watch the crew: tell the person what you would do instead.".into())
            }
            Gate::Run => {
                let by = crew.by_at_level(by, oversight);
                let done = crew.perform(&action, by).await?;
                Ok(json!({ "done": done }))
            }
            Gate::Suggest(why) => {
                if action.starts_session() {
                    crew.start_suggestable(&by, oversight).await?;
                }
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

/// What a local overseer may ask of its group's sessions on a machine,
/// through the relay: by a session id of the machine's (`devbox/<id>`).
const RELAYED: [&str; 9] = [
    "crew.history",
    "crew.nudge",
    "crew.approve",
    "crew.deny",
    "crew.stop",
    "crew.interrupt",
    "crew.resume",
    "crew.restart",
    "crew.release",
];

impl CrewTool {
    /// The group its scope is, when it is one: a machine's sessions are
    /// relayed only for a group's boss for now.
    fn group(&self) -> Option<&str> {
        self.scope.group()
    }

    /// The machines whose sessions are in its group, by the sidebar.
    fn machines(&self, crew: &CrewHub) -> Vec<String> {
        let Some(g) = self.group() else {
            return Vec::new();
        };
        let layout = crew.desk.layout();
        let mut out: Vec<String> = layout
            .groups
            .iter()
            .filter(|x| x.id == g)
            .flat_map(|x| x.items.iter())
            .filter_map(|i| i.strip_prefix("session:"))
            .filter_map(super::relay::machine_session)
            .map(|(m, _)| m.to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// `local`, the overview of this Mac, with each machine's part of its
    /// group, asked through the relay at once.
    async fn with_machines(
        &self,
        crew: &Arc<CrewHub>,
        b: &Bound,
        boss: &str,
        local: Value,
    ) -> Value {
        let (Some(g), machines) = (self.group(), self.machines(crew)) else {
            return local;
        };
        let ceiling = super::served::Ceiling {
            level: b.oversight.level,
            act: b.oversight.act,
        };
        let ceiling = &ceiling;
        let asks = machines.iter().map(|m| {
            let crew = Arc::clone(crew);
            async move {
                let part = crew
                    .relay(m, g, "crew.overview", json!({}), boss, ceiling)
                    .await;
                (m.clone(), part)
            }
        });
        let parts = futures::future::join_all(asks).await;
        super::relay::merge_overview(local, parts)
    }

    /// The call, when it names one of a machine's sessions: relayed there,
    /// decided there under its grant for the group, the boss this tool's.
    /// Nothing of it is done or judged here. None when it names none.
    async fn relayed(
        &self,
        crew: &Arc<CrewHub>,
        b: &Bound,
        input: &Value,
    ) -> Option<Result<Value, String>> {
        let named = input["session_id"].as_str()?;
        let (machine, id) = super::relay::machine_session(named)?;
        if !RELAYED.contains(&self.name) {
            return Some(Err(format!(
                "{} can't be done to a session on {machine} yet; tell the person instead.",
                self.name
            )));
        }
        let Some(g) = self.group() else {
            return Some(Err(
                "Sessions on a machine are run only by their group's boss for now.".into(),
            ));
        };
        // In its own group by the sidebar, as the machine's grant will also
        // say: nothing outside it is sent anywhere.
        let item = format!("session:{machine}/{id}");
        if crew.desk.group_of_member(&item).as_deref() != Some(g) {
            return Some(Err(crew.not_ours(&self.scope)));
        }
        // What its own level here doesn't let it do, it doesn't ask of a
        // machine either; the machine decides at no more than it.
        if self.name != "crew.history"
            && b.oversight.level == workforce_common::oversight::Level::Watch
        {
            return Some(Err(
                "You only watch the crew: tell the person what you would do instead.".into(),
            ));
        }
        let ceiling = super::served::Ceiling {
            level: b.oversight.level,
            act: b.oversight.act,
        };
        let mut sent = input.clone();
        sent["session_id"] = json!(id.to_string());
        let agent = b.service.agent_id.as_str();
        Some(
            match crew
                .relay(machine, g, self.name, sent, agent, &ceiling)
                .await
            {
                Ok(a) => {
                    let said = reported(
                        &workforce_desk::sees::capped(
                            &a.body.to_string(),
                            workforce_desk::sees::MAX_CHARS,
                        ),
                        &format!("{machine}'s answer"),
                    );
                    match a.status {
                        super::served::ServedStatus::Refused => Err(said),
                        super::served::ServedStatus::Ok => Ok(json!({ "done": said })),
                        super::served::ServedStatus::Suggested => Ok(json!({
                            "suggested": said,
                            "note": format!("Not done: {machine} keeps it for the person to decide."),
                        })),
                    }
                }
                Err(why) => Err(why),
            },
        )
    }
}

/// What the `crew.*` tool `name` is for, as the agent reads it: this hub's
/// own words, the same whether its crew is here or on a linked engine.
pub(crate) fn describe(name: &str) -> &'static str {
    match name {
            "crew.overview" => "Everyone on the crew now: the person's agents and their Claude Code and Codex sessions, each with its state (needs you, working, quiet, done today, idle, failed, stopped), what it's doing, what it asks (with approval ids), since when, and whether you run it for the person (and whether they are driving it now). slots says how many of the sessions you run are starting or working, of the most you may run at once. For what sessions spent (tokens, cost), use crew.spend.",
            super::spend::NAME => super::spend::DESCRIPTION,
            super::board::NAME => super::board::DESCRIPTION,
            super::plan::NAME => super::plan::DESCRIPTION,
            super::plan::UPDATE => super::plan::UPDATE_DESCRIPTION,
            super::handover::NAME => super::handover::DESCRIPTION,
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
            "crew.start_session" => "Start a real, visible session (Claude Code, Codex) in a project folder with a goal: it shows on the person's Floor and sidebar, they can watch it, type into it and take it over, and you run it from the start (its turn ends wake you). engine defaults to your default engine; worktree names a new branch to work on in a worktree of its own. You run at most your limit of sessions starting or working at once (crew.overview's slots).",
            "crew.assign" => "Hand a task to one of the crew's agents, in its DM.",
            "crew.assign_ticket" => "Hand a ticket on the board (ticket, its key like ACM-12) to a session (to: { \"session\": id }), an agent (to: { \"agent\": id }) or a new session started for it, which you run (to: { \"newSession\": { \"folder\": path, \"engine\": optional } }). The ticket moves to Doing and its holder is told. Only a ready ticket can be handed out (crew.board); a new session counts against your limit of sessions at once. Handing out work is for what the person has let you hand out.",
            "crew.move_ticket" => "Move a ticket on the board (ticket, its key like ACM-12) to a column (state): backlog, todo, doing, review or done. Dropping a ticket is always the person's call, so it becomes a suggestion for them.",
            "crew.take_on" => "Run a session for the person (session_id): it wakes you each time its turn ends, so you can tell it the next step. You take it on yourself only when the person lets you take on every session; otherwise this offers it to them. The person can take over any time by typing in it; it is theirs while they are active there (a couple of minutes after their last input, unless they pin it), and your messages wait until then.",
            "crew.release" => "Let a session you run go (session_id): the person runs it on their own, and its turn ends no longer wake you. Say why in reason.",
            "crew.handoff" => "Suggest the person continue a session's work in another engine (session_id; engine: claude or codex; account optional): Desk starts a session of that engine in the same folder and group with a brief of the work, and the two become one piece of work. It is only ever a suggestion the person accepts, because the brief goes to that engine's vendor, and only when they have switched it on for you.",
            _ => "",
        }
}

/// The input of the `crew.*` tool `name`; `engines` are the engines a
/// session may be started on.
pub(crate) fn schema(name: &str, engines: impl Fn() -> Vec<&'static str>) -> Value {
    let act = |mut props: serde_json::Map<String, Value>, required: &[&str]| {
        props.insert("reason".into(), reason_schema());
        let mut req: Vec<&str> = required.to_vec();
        req.push("reason");
        json!({ "type": "object", "properties": props, "required": req })
    };
    let props = |v: Value| v.as_object().cloned().unwrap_or_default();
    match name {
        "crew.overview" => json!({ "type": "object", "properties": {} }),
        super::spend::NAME => super::spend::schema(),
        super::board::NAME => super::board::schema(),
        super::plan::NAME => super::plan::schema(),
        super::plan::UPDATE => super::plan::update_schema(),
        super::handover::NAME => super::handover::schema(),
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
        "crew.handoff" => act(
            props(json!({
                "session_id": session_schema(),
                "engine": { "type": "string", "enum": ["claude", "codex"], "description": "The engine to continue in" },
                "account": { "type": "string", "description": "Optional: an account's id from crew.overview, or \"own\" for the engine's own sign-in on this computer; the engine's default when left out" }
            })),
            &["session_id", "engine"],
        ),
        "crew.start_session" => {
            let engines = engines();
            act(
                props(json!({
                    "folder": { "type": "string", "description": "Absolute path of the project folder" },
                    "goal": { "type": "string", "description": "What it is to do: its first prompt" },
                    "name": { "type": "string" },
                    "engine": { "type": "string", "enum": engines },
                    "view": { "type": "string", "enum": ["terminal"], "description": "Where it runs (default terminal)" },
                    "worktree": { "type": "string", "description": "A new branch to work on, in a worktree of its own" }
                })),
                &["folder", "goal"],
            )
        }
        "crew.assign" => act(
            props(json!({
                "agent_id": { "type": "string" },
                "task": { "type": "string" }
            })),
            &["agent_id", "task"],
        ),
        "crew.assign_ticket" => {
            let engines = engines();
            act(
                props(json!({
                    "ticket": ticket_schema(),
                    "to": { "type": "object", "description": "Who gets it: exactly one of session, agent or newSession",
                        "properties": {
                            "session": session_schema(),
                            "agent": { "type": "string", "description": "An agent id from crew.overview" },
                            "newSession": { "type": "object", "properties": {
                                "folder": { "type": "string", "description": "Absolute path of the project folder" },
                                "engine": { "type": "string", "enum": engines }
                            }, "required": ["folder"] }
                        }
                    }
                })),
                &["ticket", "to"],
            )
        }
        "crew.move_ticket" => act(
            props(json!({
                "ticket": ticket_schema(),
                "state": { "type": "string", "enum": ["backlog", "todo", "doing", "review", "done", "dropped"] }
            })),
            &["ticket", "state"],
        ),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

#[async_trait]
impl Tool for CrewTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        describe(self.name)
    }

    fn input_schema(&self) -> Value {
        schema(self.name, || {
            self.crew
                .upgrade()
                .map(|c| c.engines_to_start())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(workforce_desk::engine::startable)
        })
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
        // Only while it still oversees this scope.
        let Some((service, oversight)) = crew
            .overseer_named(&self.agent)
            .filter(|(_, o)| o.scope == self.scope)
        else {
            return Ok(ToolResult::failure(
                "You don’t oversee the crew any more.".to_string(),
                ms(),
            ));
        };
        let bound = Bound {
            service,
            oversight,
            seen: crew.desk.seen_by(&self.scope),
        };
        let result = if let Some(relayed) = self.relayed(&crew, &bound, &input).await {
            relayed
        } else if READ.contains(&self.name) {
            self.read(&crew, &bound, &input).await
        } else if [super::plan::NAME, super::plan::UPDATE].contains(&self.name) {
            crew.plan_tool(self.name, &bound, &input).await
        } else if self.name == super::handover::NAME {
            crew.handover_tool(&bound, &input).await
        } else {
            self.act(&crew, &bound, &input).await
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
            group_id: group.map(Into::into),
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
