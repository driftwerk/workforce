//! What the overseer can do to the crew, whether its level lets it, and
//! doing it — for the overseer's tools, and for the person accepting one of
//! its suggestions.

use std::path::{Component, Path};

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::crew::{self, By, Kind, Party};
use workforce_desk::model::{Engine, NewSession, PendingApproval};
use workforce_desk::Speaker;

use super::CrewHub;

/// Who a nudge goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Session(Uuid),
    Agent(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Nudge {
        to: Target,
        message: String,
    },
    Approve {
        session: Uuid,
        approval: Uuid,
    },
    Deny {
        session: Uuid,
        approval: Uuid,
    },
    Stop {
        session: Uuid,
    },
    /// End the turn it is on (Esc in its terminal); it runs on.
    Interrupt {
        session: Uuid,
    },
    Resume {
        session: Uuid,
        prompt: Option<String>,
    },
    Restart {
        session: Uuid,
        prompt: Option<String>,
    },
    StartSession {
        path: String,
        goal: String,
        name: Option<String>,
        engine: Option<Engine>,
    },
    Assign {
        agent: String,
        task: String,
    },
}

#[derive(Deserialize)]
struct Input {
    session_id: Option<String>,
    agent_id: Option<String>,
    approval_id: Option<String>,
    message: Option<String>,
    prompt: Option<String>,
    path: Option<String>,
    goal: Option<String>,
    name: Option<String>,
    engine: Option<Engine>,
    task: Option<String>,
}

fn id(raw: Option<&str>, what: &str) -> Result<Uuid, String> {
    raw.ok_or_else(|| format!("Say which {what}."))?
        .trim()
        .parse()
        .map_err(|_| format!("That isn’t a {what} id from crew.overview."))
}

fn text(raw: Option<String>, what: &str) -> Result<String, String> {
    raw.map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("Say {what}."))
}

fn blank_none(raw: Option<String>) -> Option<String> {
    raw.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

impl Action {
    /// The action a `crew.*` tool's input asks for.
    pub(crate) fn parse(tool: &str, input: &Value) -> Result<Self, String> {
        let i: Input = serde_json::from_value(input.clone()).map_err(|e| e.to_string())?;
        let session = || id(i.session_id.as_deref(), "session");
        Ok(match tool {
            "crew.nudge" => {
                let to = match (&i.session_id, &i.agent_id) {
                    (Some(_), _) => Target::Session(session()?),
                    (None, Some(a)) => Target::Agent(a.trim().to_string()),
                    (None, None) => return Err("Say which session or agent to nudge.".into()),
                };
                Self::Nudge {
                    to,
                    message: text(i.message, "what to ask")?,
                }
            }
            "crew.approve" | "crew.deny" => {
                let session = session()?;
                let approval = id(i.approval_id.as_deref(), "approval")?;
                if tool == "crew.approve" {
                    Self::Approve { session, approval }
                } else {
                    Self::Deny { session, approval }
                }
            }
            "crew.stop" => Self::Stop {
                session: session()?,
            },
            "crew.interrupt" => Self::Interrupt {
                session: session()?,
            },
            "crew.resume" => Self::Resume {
                session: session()?,
                prompt: blank_none(i.prompt),
            },
            "crew.restart" => Self::Restart {
                session: session()?,
                prompt: blank_none(i.prompt),
            },
            "crew.start_session" => Self::StartSession {
                path: text(i.path, "the project's folder")?,
                goal: text(i.goal, "the goal")?,
                name: blank_none(i.name),
                engine: i.engine,
            },
            "crew.assign" => Self::Assign {
                agent: text(i.agent_id, "which agent")?,
                task: text(i.task, "the task")?,
            },
            other => return Err(format!("{other} is not something the overseer can do")),
        })
    }

    /// The action a suggestion holds.
    pub(crate) fn from_suggestion(action: &Value) -> Result<Self, String> {
        let tool = action["tool"]
            .as_str()
            .ok_or("That suggestion doesn’t say what to do.")?;
        Self::parse(tool, &action["input"])
    }

    /// Whether doing it could push code, delete something, or reach outside
    /// Desk: never done without the person, whatever the settings.
    fn outward(&self) -> bool {
        let said = match self {
            Self::Nudge { message, .. } => message.as_str(),
            Self::Resume { prompt, .. } | Self::Restart { prompt, .. } => {
                prompt.as_deref().unwrap_or_default()
            }
            Self::StartSession { goal, .. } => goal.as_str(),
            Self::Assign { task, .. } => task.as_str(),
            Self::Approve { .. }
            | Self::Deny { .. }
            | Self::Stop { .. }
            | Self::Interrupt { .. } => "",
        };
        outward(said)
    }
}

const OUTWARD: &[&str] = &[
    "push", "pushes", "deploy", "deploys", "publish", "release", "merge", "delete", "deletes",
    "remove", "rm", "drop", "destroy", "wipe", "send", "email", "mail", "post", "tweet", "slack",
    "message", "pay", "purchase", "buy",
];

/// Whether `text` asks for something that pushes, deletes or sends.
pub(crate) fn outward(text: &str) -> bool {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| OUTWARD.contains(&w))
}

const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS", "NotebookRead"];
const READ_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "pwd", "tree", "file", "stat", "du",
    "diff", "git",
];
const READ_GIT: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "branch",
    "rev-parse",
    "ls-files",
    "blame",
];

/// Whether `path` (as a request names it) stays inside `cwd`.
fn inside(cwd: &Path, path: &str) -> bool {
    let p = Path::new(path);
    if p.components().any(|c| c == Component::ParentDir) || path.starts_with('~') {
        return false;
    }
    !p.is_absolute() || p.starts_with(cwd)
}

/// Whether a session's request only reads, and only inside its folder.
/// Anything Desk can't see all of (a clipped command) is not.
pub(crate) fn read_only_in(cwd: &str, pending: &PendingApproval) -> bool {
    let cwd = Path::new(cwd);
    let detail = pending.detail.trim();
    if detail.ends_with('…') {
        return false;
    }
    let name = pending.tool.split('(').next().unwrap_or_default().trim();
    if READ_TOOLS.contains(&name) {
        return detail.split_whitespace().all(|w| inside(cwd, w));
    }
    if name != "Bash" || detail.is_empty() {
        return false;
    }
    if detail
        .chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '\\'))
    {
        return false;
    }
    let words: Vec<&str> = detail.split_whitespace().collect();
    let Some(program) = words.first() else {
        return false;
    };
    if !READ_COMMANDS.contains(program) {
        return false;
    }
    if *program == "git" && !words.get(1).is_some_and(|sub| READ_GIT.contains(sub)) {
        return false;
    }
    if *program == "find"
        && words.iter().any(|w| {
            matches!(
                *w,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprintf"
                    | "-fls"
            )
        })
    {
        return false;
    }
    words[1..]
        .iter()
        .filter(|w| !w.starts_with('-'))
        .all(|w| inside(cwd, w.trim_matches(|c| c == '"' || c == '\'')))
}

/// Whether the overseer may do it now, or should suggest it instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gate {
    Run,
    /// Suggest it; why, for the overseer.
    Suggest(&'static str),
    /// It may not even suggest (level `watch`).
    Refuse,
}

impl CrewHub {
    /// What `level` and `[oversight.act]` let the overseer do with `action`.
    pub(crate) fn gate(&self, action: &Action, o: &Oversight) -> Gate {
        match o.level {
            Level::Watch => return Gate::Refuse,
            Level::Suggest => return Gate::Suggest("you suggest; the person decides"),
            Level::Act => {}
        }
        if action.outward() {
            return Gate::Suggest(
                "anything that pushes, deletes or reaches outside Desk is the person's call",
            );
        }
        let allowed = match action {
            Action::Nudge { .. } => o.act.nudge,
            Action::Resume { .. } | Action::Restart { .. } => o.act.resume_restart,
            Action::StartSession { .. } | Action::Assign { .. } => o.act.hand_out_work,
            Action::Approve { session, approval } => {
                if !o.act.read_only_approvals {
                    false
                } else {
                    let view = self.desk.view(*session).ok();
                    let ok = view.as_ref().is_some_and(|v| {
                        v.pending
                            .as_ref()
                            .is_some_and(|p| p.id == *approval && read_only_in(&v.cwd, p))
                    });
                    if !ok {
                        return Gate::Suggest(
                            "only requests that just read inside the session's folder are allowed without the person",
                        );
                    }
                    true
                }
            }
            Action::Interrupt { .. } => o.act.interrupt,
            Action::Deny { .. } | Action::Stop { .. } => false,
        };
        if allowed {
            Gate::Run
        } else {
            Gate::Suggest("the person hasn't let you do that without asking")
        }
    }

    fn session_name(&self, id: Uuid) -> String {
        self.desk
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "that session".into())
    }

    fn agent_name(&self, id: &str) -> String {
        self.party_of_agent(id)
            .map(|p| p.name)
            .unwrap_or_else(|| id.to_string())
    }

    /// What the action is about.
    pub(crate) fn subject_of(&self, action: &Action) -> Option<Party> {
        match action {
            Action::Nudge {
                to: Target::Agent(a),
                ..
            }
            | Action::Assign { agent: a, .. } => self.party_of_agent(a),
            Action::StartSession { .. } => None,
            Action::Nudge {
                to: Target::Session(s),
                ..
            }
            | Action::Approve { session: s, .. }
            | Action::Deny { session: s, .. }
            | Action::Stop { session: s }
            | Action::Interrupt { session: s }
            | Action::Resume { session: s, .. }
            | Action::Restart { session: s, .. } => {
                self.desk.view(*s).ok().map(|v| Party::session(&v))
            }
        }
    }

    /// The action in plain words, naming who it is for: a suggestion's text.
    pub(crate) fn describe(&self, action: &Action) -> String {
        let request = |s: &Uuid| {
            self.desk
                .view(*s)
                .ok()
                .and_then(|v| v.pending)
                .map(|p| {
                    let what = crew::approval_summary(&p.tool, &p.detail);
                    what.trim_end_matches('?').to_string()
                })
                .unwrap_or_else(|| "go ahead".into())
        };
        match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
            } => format!(
                "Ask {}: “{}”",
                self.session_name(*s),
                crew::clip(message, 200)
            ),
            Action::Nudge {
                to: Target::Agent(a),
                message,
            } => format!("Ask {}: “{}”", self.agent_name(a), crew::clip(message, 200)),
            Action::Approve { session, .. } => format!(
                "Allow {}: {}",
                self.session_name(*session),
                request(session)
            ),
            Action::Deny { session, .. } => {
                format!("Deny {}: {}", self.session_name(*session), request(session))
            }
            Action::Stop { session } => format!("Stop {}", self.session_name(*session)),
            Action::Interrupt { session } => {
                format!("Interrupt {}", self.session_name(*session))
            }
            Action::Resume { session, prompt } => match prompt {
                Some(p) => format!(
                    "Resume {} with “{}”",
                    self.session_name(*session),
                    crew::clip(p, 200)
                ),
                None => format!("Resume {}", self.session_name(*session)),
            },
            Action::Restart { session, prompt } => match prompt {
                Some(p) => format!(
                    "Restart {} with “{}”",
                    self.session_name(*session),
                    crew::clip(p, 200)
                ),
                None => format!("Restart {}", self.session_name(*session)),
            },
            Action::StartSession { path, goal, .. } => {
                format!("Start a session in {path} to {}", crew::clip(goal, 200))
            }
            Action::Assign { agent, task } => {
                format!(
                    "Give {} this: “{}”",
                    self.agent_name(agent),
                    crew::clip(task, 200)
                )
            }
        }
    }

    /// Does `action` as `by`. What happened, or what went wrong in plain
    /// words.
    pub(crate) async fn perform(&self, action: &Action, by: By) -> Result<String, String> {
        let plain = |e: workforce_desk::DeskError| match e {
            workforce_desk::DeskError::NotFound => "That session isn’t there any more.".to_string(),
            workforce_desk::DeskError::Internal(e) => {
                tracing::warn!(error = %e, "a crew action failed");
                "Something went wrong on this Mac.".to_string()
            }
            other => {
                let s = other.to_string();
                let mut c = s.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect::<String>() + ".")
                    .unwrap_or(s)
            }
        };
        let d = &self.desk;
        match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
            } => {
                let v = d.prompt(*s, message, Some(by)).map_err(plain)?;
                Ok(format!("asked {}", v.label()))
            }
            Action::Nudge {
                to: Target::Agent(a),
                message,
            } => self.message_agent(a, message, by, Kind::Nudged).await,
            Action::Assign { agent, task } => {
                self.message_agent(agent, task, by, Kind::Assigned).await
            }
            Action::Approve { session, approval } => {
                let v = d
                    .decide(
                        *session,
                        *approval,
                        workforce_desk::model::Decision::Allow,
                        Some(by),
                    )
                    .map_err(plain)?;
                Ok(format!("allowed {}", v.label()))
            }
            Action::Deny { session, approval } => {
                let v = d
                    .decide(
                        *session,
                        *approval,
                        workforce_desk::model::Decision::Deny,
                        Some(by),
                    )
                    .map_err(plain)?;
                Ok(format!("denied {}", v.label()))
            }
            Action::Stop { session } => {
                // The shell a tracked session runs in is the person's.
                if d.view(*session).is_ok_and(|v| v.terminal_id.is_some()) {
                    return Err("That session runs in the person's own Desk terminal, so it can't be stopped; crew.interrupt ends its turn and leaves the shell theirs.".into());
                }
                let v = d.stop(*session, Some(by)).map_err(plain)?;
                Ok(format!("stopped {}", v.label()))
            }
            Action::Interrupt { session } => {
                let v = d.interrupt(*session, Some(by)).map_err(plain)?;
                Ok(format!("interrupted {}", v.label()))
            }
            Action::Resume { session, prompt } => {
                let v = d
                    .resume(*session, prompt.clone(), Some(by))
                    .await
                    .map_err(plain)?;
                Ok(format!("resumed {}", v.label()))
            }
            Action::Restart { session, prompt } => {
                let v = d
                    .restart(*session, prompt.clone(), by)
                    .await
                    .map_err(plain)?;
                Ok(format!("restarted {}", v.label()))
            }
            Action::StartSession {
                path,
                goal,
                name,
                engine,
            } => {
                let folder = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "session".into());
                let v = d
                    .create(
                        NewSession {
                            engine: engine.unwrap_or(Engine::Claude),
                            view: None,
                            name: name.clone().unwrap_or(folder),
                            cwd: path.clone(),
                            goal: goal.clone(),
                            worktree: None,
                            prompt: Some(goal.clone()),
                            approvals: None,
                            env: Default::default(),
                        },
                        Some(by),
                    )
                    .await
                    .map_err(plain)?;
                Ok(format!("started {} ({})", v.label(), v.id))
            }
        }
    }

    /// Sends `text` into `agent`'s Desk DM with the person, as the overseer
    /// (or you), held to the loop guard; the turn runs on its own.
    async fn message_agent(
        &self,
        agent: &str,
        text: &str,
        by: By,
        kind: Kind,
    ) -> Result<String, String> {
        let service = self
            .state
            .registry
            .get(agent)
            .ok_or_else(|| format!("There’s no agent called {agent} on this Mac."))?;
        let name = super::name_of(&service);
        let from_overseer = by.party.kind == crew::PartyKind::Manager;
        if from_overseer {
            if by.hop > super::MAX_HOPS {
                return Err(format!(
                    "Loop guard: agents have handed this on {} times already; tell the person instead.",
                    super::MAX_HOPS
                ));
            }
            let mut sent = self.sent.lock();
            if sent.len() >= super::TURN_BUDGET {
                return Err(format!(
                    "Loop guard: you have messaged the crew {} times since the person last spoke; tell the person instead.",
                    super::TURN_BUDGET
                ));
            }
            sent.push(chrono::Utc::now());
        }
        let owner = self
            .person(Some(&service))
            .ok_or_else(|| format!("No one has talked with {name} in Desk yet, so there’s no conversation to send this to."))?;
        let party = Party::agent(agent, &name);
        let (verb, prefix) = match kind {
            Kind::Assigned => ("gave", "has a task for you"),
            _ => ("asked", "asks"),
        };
        let event = by.event(
            kind,
            Some(party),
            format!("{verb} {name} “{}”", crew::one_line(text, 120)),
        );
        let hop = by.hop;
        // Worded for the agent reading it: the overseer by its name, the
        // person (saying yes to a suggestion) by theirs.
        let (author, says) = if from_overseer {
            (
                Speaker::overseer(&by.party.id, &by.party.name),
                by.party.name.clone(),
            )
        } else {
            (Speaker::you(), self.person_name(&owner))
        };
        let message = format!("{says} {prefix}: {text}");
        self.desk.note(event);
        let crew = self.me();
        tokio::spawn(async move {
            if let Err(e) = crew
                .run_dm_turn(&service, &owner, &message, hop, None, Some(author))
                .await
            {
                tracing::warn!(agent_id = %service.agent_id, error = %e, "a message to the crew did not run");
            }
        });
        Ok(format!("{verb} {name}"))
    }

    /// What the agents call the person `owner`: their name, when the hub
    /// knows them by one.
    pub(super) fn person_name(&self, owner: &str) -> String {
        crate::desk_surface::person_name(self.state.identity_store.as_ref(), owner)
            .unwrap_or_else(|| "The person you work for".into())
    }

    /// The suggestion `action` becomes, recorded and announced.
    pub(crate) async fn suggest(
        &self,
        tool: &str,
        input: &Value,
        action: &Action,
        by: &By,
    ) -> Result<crew::Suggestion, String> {
        let mut input = input.clone();
        if let Some(o) = input.as_object_mut() {
            o.remove("reason");
        }
        let suggestion = crew::Suggestion {
            id: crew::new_id(),
            at: crew::now(),
            text: self.describe(action),
            action: json!({ "tool": tool, "input": input }),
            state: crew::SuggestionState::Open,
            subject: self.subject_of(action),
            agent_id: by.party.id.clone(),
        };
        self.desk
            .store()
            .put_suggestion(&suggestion)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "saving a suggestion failed");
                "The suggestion could not be saved.".to_string()
            })?;
        let mut b = by.clone();
        b.suggestion_id = Some(suggestion.id.clone());
        self.desk.note(b.event(
            Kind::Suggested,
            suggestion.subject.clone(),
            format!("suggested: {}", suggestion.text),
        ));
        Ok(suggestion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(tool: &str, detail: &str) -> PendingApproval {
        PendingApproval {
            id: Uuid::new_v4(),
            tool: tool.into(),
            detail: detail.into(),
            since: chrono::Utc::now(),
            answerable: true,
        }
    }

    #[test]
    fn only_reads_inside_the_folder_are_read_only() {
        let cwd = "/Users/k/dev/web";
        for (tool, detail) in [
            (
                "Read(/Users/k/dev/web/src/a.ts)",
                "/Users/k/dev/web/src/a.ts",
            ),
            ("Bash(git status)", "git status"),
            ("Bash(ls src)", "ls src"),
            ("Bash(rg TODO src)", "rg TODO src"),
            ("Grep(TODO)", "TODO"),
        ] {
            assert!(read_only_in(cwd, &pending(tool, detail)), "{detail}");
        }
        for (tool, detail) in [
            ("Read(/etc/passwd)", "/etc/passwd"),
            ("Read(../secrets)", "../secrets"),
            ("Bash(git push)", "git push"),
            ("Bash(rm -rf target)", "rm -rf target"),
            ("Bash(cat a | sh)", "cat a | sh"),
            ("Bash(ls > out)", "ls > out"),
            ("Bash(find . -delete)", "find . -delete"),
            ("Bash(cat ~/.ssh/id)", "cat ~/.ssh/id"),
            ("Bash(npm install)", "npm install"),
            ("Edit(src/a.ts)", "src/a.ts"),
            ("Bash(ls aaaa…)", "ls aaaa…"),
        ] {
            assert!(!read_only_in(cwd, &pending(tool, detail)), "{detail}");
        }
    }

    #[test]
    fn pushing_deleting_and_sending_are_outward() {
        assert!(outward("Push the branch when green"));
        assert!(outward("rm the build folder"));
        assert!(outward("email the team"));
        assert!(!outward("Run the tests without the network ones"));
        assert!(!outward("Still on the provider tests, or stuck?"));
    }

    #[test]
    fn actions_parse_from_tool_input_and_back_from_a_suggestion() {
        let s = Uuid::new_v4();
        let a = Action::parse(
            "crew.restart",
            &json!({"session_id": s.to_string(), "prompt": "Skip network tests.", "reason": "stuck"}),
        )
        .unwrap();
        assert_eq!(
            a,
            Action::Restart {
                session: s,
                prompt: Some("Skip network tests.".into())
            }
        );
        let back = Action::from_suggestion(
            &json!({"tool": "crew.restart", "input": {"session_id": s.to_string(), "prompt": "Skip network tests."}}),
        )
        .unwrap();
        assert_eq!(back, a);
        assert!(Action::parse("crew.nudge", &json!({"message": "hi"})).is_err());
        assert_eq!(
            Action::parse("crew.nudge", &json!({"agent_id": "scout", "message": "hi"})).unwrap(),
            Action::Nudge {
                to: Target::Agent("scout".into()),
                message: "hi".into()
            }
        );
    }
}
