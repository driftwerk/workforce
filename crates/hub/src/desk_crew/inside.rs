//! `crew.inside`: the boss sees inside one of its sessions itself, read-only
//! and without asking it. Desk's own evidence: what runs under it and what
//! that waits on (its background sample), its transcript, its folder and
//! its pull request, and its own lines in Desk's logs.
//!
//! Only the person's own sessions are read. A Desk is one person's, so its
//! session records carry no owner: the boundary is what the boss's scope
//! sees ([`CrewHub::session_seen`]) here, and its grant on a machine.
//! Another person's sessions are on their own Desk, which nothing here
//! reaches.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use serde_json::{json, Value};
use workforce_common::oversight::Oversight;
use workforce_desk::grant::{Grant, Sees};
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::sees::{self, Part};
use workforce_desk::sight::{self, Glance, PullGlance, Thresholds, WorkspaceGlance};
use workforce_desk::{Processes, TranscriptAsk};

use super::github_seen::{Pull, Seen, Unseen};
use super::served::ServedStatus;
use super::tools::Bound;
use super::watcher::reported;
use super::workspace::{self, Workspace};
use super::{AgentService, CrewHub};

pub const NAME: &str = "crew.inside";

pub const DESCRIPTION: &str = "See inside one session yourself, read-only, without asking it \
(session_id). facet glance (the default) says what it is doing now from evidence: what runs under \
it and for how long, what that waits on, when its transcript last grew, its branch and unpushed \
work, and its PR's CI. transcript pages its own conversation newest first, every tool call with \
its input and output (before: the cursor a page gave; query: search it; step: one call's output \
whole). processes, workspace and logs give each of those in full. Read it before crew.account's \
question, and long before crew.ask_session.";

/// Every facet, the default first.
const FACETS: [&str; 5] = ["glance", "transcript", "processes", "workspace", "logs"];
/// How long a folder's read serves a glance.
const WORKSPACE_FOR: Duration = Duration::from_secs(60);
/// The most sessions one glance at everything working gives.
const GLANCES_MOST: usize = 20;
/// How many of those are read at once.
const GLANCES_AT_ONCE: usize = 5;
/// How long one session's folder and PR are waited on before its glance is
/// said from Desk's sample alone.
const GLANCE_READ_FOR: Duration = Duration::from_secs(10);
/// The most uncommitted paths the workspace facet names.
const UNCOMMITTED_NAMED: usize = 20;
/// What every session the boss sees means, with the glance facet.
const EVERY: &str = "*";
/// How long a machine whose Desk is older than this tool isn't asked
/// again.
const OLDER_FOR: Duration = Duration::from_secs(10 * 60);
/// How often one machine's grant is asked for its glances, at most.
const MACHINE_GLANCE_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long a machine session's last glance stands while it can't be read
/// again.
const MACHINE_GLANCE_FOR: Duration = Duration::from_secs(15 * 60);

const NOT_A_SESSION: &str = "That isn't a session id from crew.overview. An agent's own work is \
read with crew.history (agent_id).";
const NOT_RUNNING: &str = "Nothing of it runs on this computer now.";
const UNLISTED: &str = "Desk can't list processes on this computer.";
const NO_REPOSITORY: &str = "Its folder holds no git repository.";
const UNREAD: &str = "Its folder couldn't be read.";
const NOT_GITHUB: &str = "Not a GitHub repository.";
const DETACHED: &str = "Its HEAD is detached, so no PR is looked for.";
const PULL_UNREAD: &str = "Its PR couldn't be read from GitHub just now.";
const NAMES_HIDDEN: &str =
    "Desk can't hide secrets on this computer, so it names no files or commit subjects.";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "session_id": { "type": "string", "description": "A session id from crew.overview (devbox/<id> for one on a machine), or * with facet glance for every one working" },
        "facet": { "type": "string", "enum": FACETS },
        "query": { "type": "string", "description": "transcript or logs: words to search for" },
        "before": { "type": "string", "description": "transcript: the cursor a page ended with, for the page before it" },
        "step": { "type": "string", "description": "transcript: one tool call's id, for its output whole" },
        "record": { "type": "string", "description": "transcript: a sub-agent's record by its name, from records" }
    }, "required": ["session_id"] })
}

/// The facet `input` asks for, and the session it names: none for every
/// one working, which only a glance is.
fn asked(input: &Value) -> Result<(&'static str, Option<uuid::Uuid>), String> {
    let facet = match said(input, "facet") {
        None => FACETS[0],
        Some(f) => FACETS
            .into_iter()
            .find(|k| *k == f)
            .ok_or_else(|| format!("facet is one of {}.", FACETS.join(", ")))?,
    };
    let named =
        said(input, "session_id").ok_or("Say which session: a session_id from crew.overview.")?;
    match (named.as_str(), facet) {
        (EVERY, "glance") => Ok((facet, None)),
        (EVERY, _) => Err(format!(
            "{EVERY} is for facet glance only: name one session."
        )),
        (id, _) => id
            .parse()
            .map(|id| (facet, Some(id)))
            .map_err(|_| NOT_A_SESSION.to_string()),
    }
}

/// Whether `v` works, or waits on its own background work.
fn busy(v: &SessionView) -> bool {
    v.state == SessionState::Working || v.waiting_on() > 0
}

/// What the boss is told of a machine whose Desk can't show inside its
/// sessions.
fn older(machine: &str) -> String {
    format!(
        "{machine} runs an older Desk that can't show inside its sessions; update Desk there. \
         Until then, crew.history and crew.account tell you what it last said."
    )
}

/// `sees` in the words a refused facet says it.
fn sees_said(sees: Sees) -> &'static str {
    match sees {
        Sees::Status => "Status",
        Sees::Replies => "Replies",
        Sees::Everything => "Everything",
    }
}

/// What is known inside the sessions on the person's machines, kept on
/// this Mac.
#[derive(Debug, Default)]
pub(super) struct MachineSight {
    /// Each one's last glance, by `machine/id`, its state then, and when it
    /// came.
    glances: HashMap<String, (Glance, SessionState, Instant)>,
    /// When each machine's grant was last asked for its glances.
    asked: HashMap<(String, String), Instant>,
    /// When each plan session on a machine was last seen to move.
    moved: HashMap<String, Instant>,
    /// The machines whose Desk is older than this tool, since when.
    older: HashMap<String, Instant>,
}

/// `input[key]`, trimmed, when it says something.
fn said(input: &Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The pull request's CI as a glance says it, against the folder's HEAD.
fn pull_glance(p: &Pull, head: Option<&str>) -> PullGlance {
    PullGlance {
        number: p.number,
        checks: p.checks.clone().unwrap_or_else(|| "none".into()),
        failed: p.failed_names.clone(),
        head_matches_local: p.head.as_deref().zip(head).map(|(a, b)| a == b),
    }
}

/// What is known of a folder's pull request: it, and what of it couldn't
/// be read.
struct PullSeen {
    pull: Option<Pull>,
    note: Option<String>,
}

impl CrewHub {
    /// `crew.inside`, for a session here (one on a machine is relayed
    /// before it reaches this).
    pub(super) async fn inside_tool(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        let (facet, named) = asked(input)?;
        let Some(id) = named else {
            return Ok(self.glances(b).await);
        };
        if self.is_agent(&id.to_string()) {
            return Err(NOT_A_SESSION.into());
        }
        self.session_seen(id, &b.seen)?;
        let v = self
            .desk
            .view(id)
            .map_err(|_| self.not_ours(&b.oversight.scope))?;
        match facet {
            "glance" => {
                let g = self.glance_of(&b.service, &v, &b.oversight).await;
                Ok(json!({
                    "state": v.state,
                    "glance": self.glance_said(&g),
                }))
            }
            "transcript" => {
                let ask = TranscriptAsk {
                    query: said(input, "query"),
                    before: said(input, "before"),
                    step: said(input, "step"),
                    record: said(input, "record"),
                };
                let t = self.desk.transcript_page(id, ask).await?;
                Ok(json!({ "transcript": fence(&t, "the session's transcript") }))
            }
            "processes" => Ok(match self.desk.process_tree(id).await {
                Processes::NotRunning => json!({ "processes": NOT_RUNNING }),
                Processes::Unlisted => json!({ "processes": UNLISTED }),
                Processes::Tree(t) => json!({ "processes": fence(&t, "the session's processes") }),
            }),
            "workspace" => {
                let ws = workspace::workspace_of(&v.cwd).await;
                let r = PrRef::of(&ws);
                let pr = self.workspace_pull(&b.service, r.as_ref()).await;
                let head = r.and_then(|r| r.head);
                let said = Self::with_pull(
                    self.workspace_said(&ws, Sees::Everything),
                    pr,
                    head.as_deref(),
                    Sees::Everything,
                );
                Ok(json!({ "workspace": reported(&said.to_string(), "the session's workspace") }))
            }
            _ => {
                let logs = self.desk.session_logs(id, said(input, "query")).await?;
                Ok(json!({ "logs": fence(&logs, "the session's own log lines") }))
            }
        }
    }

    /// The glances of every session `b` sees that is working or waits on
    /// its own background work, at most [`GLANCES_MOST`].
    async fn glances(&self, b: &Bound) -> Value {
        let tz = self.desk.timezone();
        let busy: Vec<SessionView> = self
            .desk
            .list()
            .into_iter()
            .filter(|v| b.seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
            .filter(busy)
            .collect();
        let more = busy.len().saturating_sub(GLANCES_MOST);
        let lines: Vec<String> = futures::stream::iter(busy.into_iter().take(GLANCES_MOST))
            .map(|v| async move {
                let g = tokio::time::timeout(
                    GLANCE_READ_FOR,
                    self.glance_of(&b.service, &v, &b.oversight),
                )
                .await
                .unwrap_or_else(|_| self.desk.glance(&v));
                format!(
                    "{} ({}): {}",
                    v.label(),
                    v.id,
                    g.line(tz, chrono::Utc::now())
                )
            })
            .buffered(GLANCES_AT_ONCE)
            .collect()
            .await;
        let mut out = json!({
            "sessions": reported(&lines.join("\n"), "what Desk sees inside the working sessions"),
        });
        if more > 0 {
            out["more"] = json!(format!("{more} more are working: name one to see it."));
        }
        out
    }

    /// What Desk sees inside `v` now: its last sample, its folder (read at
    /// most once a minute) and its pull request as the person sees it, and
    /// the stall they show under `o`'s thresholds.
    pub(super) async fn glance_of(
        &self,
        service: &Arc<AgentService>,
        v: &SessionView,
        o: &Oversight,
    ) -> Glance {
        let ws = self.workspace_seen(&v.cwd).await;
        let r = PrRef::of(&ws);
        let mut g = self.desk.glance(v);
        g.workspace = workspace_glance(&ws);
        self.judged(g, r, service, v.state, o, Sees::Everything)
            .await
    }

    /// `g`, what is seen of a session in `state` at `sees`, with its PR as
    /// the person sees it (looked for at `r`) and the stall it shows under
    /// `o`'s thresholds.
    async fn judged(
        &self,
        g: Glance,
        r: Option<PrRef>,
        service: &Arc<AgentService>,
        state: SessionState,
        o: &Oversight,
        sees: Sees,
    ) -> Glance {
        let mut g = g;
        let pr = self.workspace_pull(service, r.as_ref()).await;
        if let Some(w) = g.workspace.as_mut() {
            add_pull(w, pr, r.as_ref().and_then(|r| r.head.as_deref()));
        }
        let mut g = sees::glance(g, sees);
        g.stall = sight::verdict(&g, state, chrono::Utc::now(), self.stall_thresholds(o));
        g
    }

    /// Whether `id` names one of this hub's agents, whatever it looks like.
    fn is_agent(&self, id: &str) -> bool {
        self.services().iter().any(|s| s.agent_id.as_str() == id)
    }

    /// `g` rendered as the boss reads it, fenced.
    pub(super) fn glance_said(&self, g: &Glance) -> String {
        reported(
            &g.render(self.desk.timezone(), chrono::Utc::now()),
            "what Desk sees inside the session",
        )
    }

    /// `o`'s quiet and stall minutes, in this hub's minutes.
    fn stall_thresholds(&self, o: &Oversight) -> Thresholds {
        let minutes = |n: u32| {
            chrono::Duration::from_std(self.timing.minute.saturating_mul(n))
                .unwrap_or(chrono::Duration::MAX)
        };
        Thresholds {
            quiet: minutes(o.wake.quiet_minutes),
            stall: minutes(o.work.stall_minutes),
        }
    }

    /// The folder `cwd` as git read it within the last [`WORKSPACE_FOR`].
    async fn workspace_seen(&self, cwd: &str) -> Workspace {
        let now = Instant::now();
        let kept = self
            .workspaces
            .lock()
            .get(cwd)
            .filter(|(_, at)| now.saturating_duration_since(*at) < WORKSPACE_FOR)
            .map(|(w, _)| w.clone());
        if let Some(w) = kept {
            return w;
        }
        let w = workspace::workspace_of(cwd).await;
        let mut kept = self.workspaces.lock();
        kept.retain(|_, (_, at)| now.saturating_duration_since(*at) < WORKSPACE_FOR);
        kept.insert(cwd.to_string(), (w.clone(), now));
        w
    }

    /// The pull request `r`'s branch is, read as the person through
    /// [`super::github_seen`]; none when there is no repository to look
    /// for one of.
    async fn workspace_pull(
        &self,
        service: &Arc<AgentService>,
        r: Option<&PrRef>,
    ) -> Option<PullSeen> {
        let r = r?;
        let note = |n: &str| {
            Some(PullSeen {
                pull: None,
                note: Some(n.to_string()),
            })
        };
        let Some(origin) = r.origin.as_deref() else {
            return note(NOT_GITHUB);
        };
        let Some(branch) = r.branch.as_deref() else {
            return note(DETACHED);
        };
        match self.branch_pull(service, origin, branch).await {
            Ok(Seen {
                value: Some(p),
                at,
                held,
            }) => Some(PullSeen {
                note: Seen {
                    value: (),
                    at,
                    held,
                }
                .note(&self.desk.timezone()),
                pull: Some(p),
            }),
            Ok(Seen { value: None, .. }) => note(&format!("No PR from this branch on {origin}.")),
            Err(Unseen::NotConnected) => note(super::github::NOT_CONNECTED),
            Err(Unseen::Held(_)) => note("GitHub is rate-limiting Desk, so its PR isn't read now."),
            Err(Unseen::Failed(e)) => {
                tracing::info!(%origin, error = %e, "a session's pull request couldn't be read");
                note(PULL_UNREAD)
            }
        }
    }

    /// The workspace facet's answer at `sees`, its PR aside: the folder
    /// whole at Everything, its counts and head at Status, its branch and
    /// commit subjects as well at Replies. Names are redacted, or left out
    /// where secrets can't be hidden.
    fn workspace_said(&self, ws: &Workspace, sees: Sees) -> Value {
        let r = match ws {
            Workspace::Nothing => return json!({ "note": NO_REPOSITORY }),
            Workspace::Unread => return json!({ "note": UNREAD }),
            Workspace::Repo(r) => r,
        };
        let replies = sees::shows(sees, Part::Reply);
        let inside = sees::shows(sees, Part::Inside);
        let hidden = self.desk.redact("").is_none();
        let redact = |t: &str| self.desk.redact(t).unwrap_or_default();
        let mut said = json!({
            "head": r.head.as_deref().map(|h| h.chars().take(12).collect::<String>()),
            "ahead": r.ahead_behind.map(|(a, _)| a),
            "behind": r.ahead_behind.map(|(_, b)| b),
            "unpushed": r.unpushed,
            "uncommitted": r.uncommitted.len(),
            "worktree": r.worktree,
            "left_behind": left(ws),
        });
        if replies {
            said["branch"] = json!(r.branch);
            said["upstream"] = json!(r.upstream);
            said["origin"] = json!(r.origin);
            said["recent"] = json!(r
                .recent
                .iter()
                .map(|c| json!({
                    "commit": c.short,
                    "at": c.at,
                    "subject": (!hidden).then(|| redact(&c.subject)),
                }))
                .collect::<Vec<_>>());
        }
        if inside && !hidden {
            said["uncommitted_files"] = json!(r
                .uncommitted
                .iter()
                .take(UNCOMMITTED_NAMED)
                .map(|f| redact(f))
                .collect::<Vec<_>>());
        }
        if hidden && replies {
            said["note"] = json!(NAMES_HIDDEN);
        }
        said
    }

    /// `said`, a workspace facet's answer, with its PR at `sees`.
    fn with_pull(mut said: Value, pr: Option<PullSeen>, head: Option<&str>, sees: Sees) -> Value {
        let Some(pr) = pr else {
            return said;
        };
        let replies = sees::shows(sees, Part::Reply);
        if let Some(p) = &pr.pull {
            if merged_at(p, head) && said["uncommitted"] == 0 {
                said["left_behind"] = Value::Null;
            }
            let mut pull = json!({
                "checks": p.checks.as_deref().unwrap_or("none"),
                "head_matches_local": pull_glance(p, head).head_matches_local,
            });
            if replies {
                pull["number"] = json!(p.number);
                pull["state"] = json!(if p.merged {
                    "merged"
                } else if p.open {
                    "open"
                } else {
                    "closed"
                });
                pull["draft"] = json!(p.draft);
                pull["failed"] = json!(p.failed_names);
                pull["url"] = json!(p.url);
            }
            said["pr"] = pull;
        }
        if let Some(n) = pr.note {
            let before = said["note"]
                .as_str()
                .map(|b| format!("{b} "))
                .unwrap_or_default();
            said["note"] = json!(format!("{before}{n}"));
        }
        said
    }
}

impl CrewHub {
    /// `crew.inside` served for the boss of `g`, a grant on this machine:
    /// one of the grant's own sessions, shown at its Sees, its PR left for
    /// the boss's home to read as the person.
    pub(super) async fn served_inside(&self, input: &Value, g: &Grant) -> Result<Value, String> {
        let (facet, named) = asked(input)?;
        let Some(id) = named else {
            let mut glances = Vec::new();
            for v in self
                .desk
                .list()
                .into_iter()
                .filter(|v| self.in_grant(g, v.id) && busy(v))
                .take(GLANCES_MOST)
            {
                glances.push(self.served_glance(&v, g.sees).await);
            }
            return Ok(json!({ "glances": glances }));
        };
        if !self.in_grant(g, id) {
            return Err(self.not_in_grant(g));
        }
        let v = self.desk.view(id).map_err(|_| self.not_in_grant(g))?;
        let inside = sees::shows(g.sees, Part::Inside);
        if !inside && matches!(facet, "transcript" | "logs") {
            return Err(format!(
                "This machine's grant for this group shows {} only, not inside its sessions.",
                sees_said(g.sees)
            ));
        }
        match facet {
            "glance" => Ok(self.served_glance(&v, g.sees).await),
            "transcript" => {
                let ask = TranscriptAsk {
                    query: said(input, "query"),
                    before: said(input, "before"),
                    step: said(input, "step"),
                    record: said(input, "record"),
                };
                let t = self.desk.transcript_page(id, ask).await?;
                Ok(json!({ "transcript": t }))
            }
            "processes" => Ok(json!({
                "processes": match sees::processes(self.desk.process_tree(id).await, g.sees) {
                    Processes::NotRunning => json!(NOT_RUNNING),
                    Processes::Unlisted => json!(UNLISTED),
                    tree => json!(tree),
                },
            })),
            "workspace" => {
                let ws = workspace::workspace_of(&v.cwd).await;
                Ok(json!({
                    "workspace": self.workspace_said(&ws, g.sees),
                    "pr_ref": PrRef::of(&ws),
                    "sees": g.sees,
                }))
            }
            _ => Ok(json!({ "logs": self.desk.session_logs(id, said(input, "query")).await? })),
        }
    }

    /// `v`'s glance at `sees`, its PR aside, and where its home looks for
    /// that PR.
    async fn served_glance(&self, v: &SessionView, sees: Sees) -> Value {
        let ws = self.workspace_seen(&v.cwd).await;
        let mut g = self.desk.glance(v);
        g.workspace = workspace_glance(&ws);
        json!({
            "id": v.id,
            "name": v.label(),
            "state": v.state,
            "glance": sees::glance(g, sees),
            "pr_ref": PrRef::of(&ws),
            "sees": sees,
        })
    }

    /// `crew.inside` asked of `machine` under its `grant` for `service`,
    /// overseeing as `o`: its answer fenced field by field, the PR read
    /// here as the person and the stall a glance shows judged here. A
    /// machine whose Desk is older than this tool is said so, and left
    /// alone for [`OLDER_FOR`].
    pub(super) async fn relayed_inside(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        machine: &str,
        grant: &str,
        input: Value,
    ) -> Result<Value, String> {
        let older_now = self
            .machine_sight
            .lock()
            .older
            .get(machine)
            .is_some_and(|at| at.elapsed() < OLDER_FOR);
        if older_now {
            return Err(older(machine));
        }
        let agent = service.agent_id.as_str();
        let ceiling = self.ceiling_now(agent, o);
        let a = self
            .relay(machine, grant, NAME, input, agent, &ceiling)
            .await?;
        match a.status {
            ServedStatus::Refused if super::relay::older(machine, NAME, &a).is_some() => {
                self.machine_sight
                    .lock()
                    .older
                    .insert(machine.to_string(), Instant::now());
                Err(older(machine))
            }
            ServedStatus::Refused => Err(reported(
                &a.body["error"]
                    .as_str()
                    .map_or_else(|| a.body.to_string(), str::to_string),
                &format!("{machine}'s answer"),
            )),
            ServedStatus::Ok | ServedStatus::Suggested => {
                Ok(self.inside_from(service, o, machine, a.body).await)
            }
        }
    }

    /// What `machine` answered of `crew.inside`, as the boss reads it.
    async fn inside_from(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        machine: &str,
        body: Value,
    ) -> Value {
        let from = format!("what Desk on {machine} sees inside the session");
        let tz = self.desk.timezone();
        if let Some(all) = body["glances"].as_array() {
            let mut lines = Vec::new();
            for one in all {
                if let Some((id, _, g)) = self.machine_glance(service, o, machine, one).await {
                    lines.push(format!(
                        "{} ({machine}/{id}): {}",
                        one["name"].as_str().unwrap_or("a session"),
                        g.line(tz, chrono::Utc::now())
                    ));
                }
            }
            return json!({ "sessions": reported(&lines.join("\n"), &from) });
        }
        if body["glance"].is_object() {
            return match self.machine_glance(service, o, machine, &body).await {
                Some((_, state, g)) => json!({
                    "state": state,
                    "glance": reported(&g.render(tz, chrono::Utc::now()), &from),
                }),
                None => json!({ "glance": reported(&body.to_string(), &from) }),
            };
        }
        if body["workspace"].is_object() {
            let r: Option<PrRef> = serde_json::from_value(body["pr_ref"].clone())
                .ok()
                .flatten();
            let sees = serde_json::from_value(body["sees"].clone()).unwrap_or_default();
            let pr = self.workspace_pull(service, r.as_ref()).await;
            let head = r.and_then(|r| r.head);
            let said = Self::with_pull(body["workspace"].clone(), pr, head.as_deref(), sees);
            return json!({ "workspace": reported(&said.to_string(), &from) });
        }
        let fields = body.as_object().into_iter().flatten().map(|(k, v)| {
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            (k.clone(), json!(reported(&text, &from)))
        });
        Value::Object(fields.collect())
    }

    /// One session's glance as `machine` gave it (`one`: its id, state,
    /// glance, Sees and where its PR is), judged here and kept for the
    /// watchers.
    async fn machine_glance(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        machine: &str,
        one: &Value,
    ) -> Option<(String, SessionState, Glance)> {
        let id = one["id"].as_str()?.to_string();
        let state: SessionState = serde_json::from_value(one["state"].clone()).ok()?;
        let g: Glance = serde_json::from_value(one["glance"].clone()).ok()?;
        let r: Option<PrRef> = serde_json::from_value(one["pr_ref"].clone()).ok().flatten();
        let sees = serde_json::from_value(one["sees"].clone()).unwrap_or_default();
        let g = self.judged(g, r, service, state, o, sees).await;
        self.machine_sight.lock().glances.insert(
            format!("{machine}/{id}"),
            (g.clone(), state, Instant::now()),
        );
        Some((id, state, g))
    }

    /// Asks `machine` for the glances of its working sessions under
    /// `grant`, for `boss`, at most once every [`MACHINE_GLANCE_EVERY`];
    /// what it gives is kept for the watchers.
    pub(super) async fn glance_machine(&self, machine: &str, grant: &str, boss: &str) {
        let key = (machine.to_string(), grant.to_string());
        {
            let mut seen = self.machine_sight.lock();
            if seen
                .asked
                .get(&key)
                .is_some_and(|at| at.elapsed() < MACHINE_GLANCE_EVERY)
            {
                return;
            }
            seen.asked.insert(key, Instant::now());
            seen.glances
                .retain(|_, (_, _, at)| at.elapsed() < MACHINE_GLANCE_FOR);
        }
        let Some((service, o)) = self.overseer_named(boss) else {
            return;
        };
        let input = json!({ "session_id": EVERY, "facet": "glance" });
        if let Err(e) = self
            .relayed_inside(&service, &o, machine, grant, input)
            .await
        {
            tracing::debug!(%machine, error = %e, "a machine's sessions weren't glanced at");
        }
    }

    /// Forgets when the machines' plan sessions that `kept` doesn't hold
    /// last moved.
    pub(super) fn machine_sight_keep(&self, kept: impl Fn(&str) -> bool) {
        self.machine_sight.lock().moved.retain(|k, _| kept(k));
    }

    /// Notes that the plan session `item` on a machine was seen to move:
    /// what was seen inside it before (its glance there, as it worked, or
    /// as it finished) no longer says what it does.
    pub(super) fn machine_moved(&self, item: &str) {
        let mut seen = self.machine_sight.lock();
        seen.moved.insert(item.to_string(), Instant::now());
        seen.glances.remove(item);
    }

    /// Whether the plan session `item` on a machine hasn't moved for
    /// `boss`'s quiet minutes.
    pub(super) fn machine_quiet(&self, item: &str, boss: &str) -> bool {
        let Some((_, o)) = self.overseer_named(boss) else {
            return false;
        };
        let quiet = self.timing.minute.saturating_mul(o.wake.quiet_minutes);
        !quiet.is_zero()
            && self
                .machine_sight
                .lock()
                .moved
                .get(item)
                .is_some_and(|at| at.elapsed() >= quiet)
    }
}

/// Where a folder's PR is looked for: its GitHub repository, branch and
/// HEAD.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PrRef {
    origin: Option<String>,
    branch: Option<String>,
    head: Option<String>,
}

impl PrRef {
    fn of(ws: &Workspace) -> Option<Self> {
        match ws {
            Workspace::Repo(r) => Some(Self {
                origin: r.origin.clone(),
                branch: r.branch.clone(),
                head: r.head.clone(),
            }),
            Workspace::Nothing | Workspace::Unread => None,
        }
    }
}

/// Whether `p` merged with `head` as its own head: its commits count as
/// pushed.
fn merged_at(p: &Pull, head: Option<&str>) -> bool {
    p.merged && p.head.is_some() && p.head.as_deref() == head
}

/// What closing `ws`'s session would leave behind, in closing's words, no
/// merged PR known.
fn left(ws: &Workspace) -> Option<String> {
    workspace::held(&ws.folder(), &[])
}

/// `ws` as a glance says it, its PR aside.
fn workspace_glance(ws: &Workspace) -> Option<WorkspaceGlance> {
    Some(match ws {
        Workspace::Nothing => return None,
        Workspace::Unread => WorkspaceGlance {
            left: left(ws),
            note: Some(UNREAD.into()),
            ..WorkspaceGlance::default()
        },
        Workspace::Repo(r) => WorkspaceGlance {
            branch: r.branch.clone(),
            unpushed: r.unpushed,
            uncommitted: r.uncommitted.len(),
            behind: r.ahead_behind.map(|(_, b)| b),
            left: left(ws),
            pr: None,
            note: None,
        },
    })
}

/// `w` with what is known of its PR, against the folder's `head`.
fn add_pull(w: &mut WorkspaceGlance, pr: Option<PullSeen>, head: Option<&str>) {
    let Some(pr) = pr else {
        return;
    };
    if let Some(p) = &pr.pull {
        if merged_at(p, head) && w.uncommitted == 0 {
            w.left = None;
        }
        w.pr = Some(pull_glance(p, head));
    }
    w.note = pr.note;
}

/// `v` as JSON, fenced as what came `from` the session.
fn fence(v: &impl serde::Serialize, from: &str) -> String {
    reported(&serde_json::to_string(v).unwrap_or_default(), from)
}
