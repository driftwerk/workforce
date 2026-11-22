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

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use workforce_common::oversight::Oversight;
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::sight::{self, Glance, PullGlance, Thresholds, WorkspaceGlance};
use workforce_desk::{Processes, TranscriptAsk};

use super::github_seen::{Pull, Seen, Unseen};
use super::tools::Bound;
use super::watcher::reported;
use super::workspace::{self, Repo, Workspace};
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
/// The most uncommitted paths the workspace facet names.
const UNCOMMITTED_NAMED: usize = 20;
/// What every session the boss sees means, with the glance facet.
const EVERY: &str = "*";

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
        let facet = said(input, "facet").unwrap_or_else(|| FACETS[0].into());
        if !FACETS.contains(&facet.as_str()) {
            return Err(format!("facet is one of {}.", FACETS.join(", ")));
        }
        let named = said(input, "session_id")
            .ok_or("Say which session: a session_id from crew.overview.")?;
        if named == EVERY {
            return match facet.as_str() {
                "glance" => Ok(self.glances(b).await),
                _ => Err(format!(
                    "{EVERY} is for facet glance only: name one session."
                )),
            };
        }
        let id: uuid::Uuid = named.parse().map_err(|_| NOT_A_SESSION.to_string())?;
        self.session_seen(id, &b.seen)?;
        let v = self
            .desk
            .view(id)
            .map_err(|_| self.not_ours(&b.oversight.scope))?;
        match facet.as_str() {
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
                let pr = self.workspace_pull(&b.service, &ws).await;
                Ok(
                    json!({ "workspace": reported(&self.workspace_said(&ws, &pr).to_string(), "the session's workspace") }),
                )
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
            .filter(|v| v.state == SessionState::Working || v.waiting_on() > 0)
            .collect();
        let more = busy.len().saturating_sub(GLANCES_MOST);
        let mut lines = Vec::new();
        for v in busy.iter().take(GLANCES_MOST) {
            let g = self.glance_of(&b.service, v, &b.oversight).await;
            lines.push(format!(
                "{} ({}): {}",
                v.label(),
                v.id,
                g.line(tz, chrono::Utc::now())
            ));
        }
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
        let now = chrono::Utc::now();
        let mut g = self.desk.glance(v);
        let ws = self.workspace_seen(&v.cwd).await;
        let pr = self.workspace_pull(service, &ws).await;
        g.workspace = workspace_glance(&ws, &pr);
        g.stall = sight::verdict(&g, v.state, now, self.stall_thresholds(o));
        g
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

    /// The pull request `ws`'s branch is, read as the person through
    /// [`super::github_seen`]; none when there is no branch on GitHub to
    /// look for.
    async fn workspace_pull(
        &self,
        service: &Arc<AgentService>,
        ws: &Workspace,
    ) -> Option<PullSeen> {
        let Workspace::Repo(repo) = ws else {
            return None;
        };
        let note = |n: &str| {
            Some(PullSeen {
                pull: None,
                note: Some(n.to_string()),
            })
        };
        let Some(origin) = repo.origin.as_deref() else {
            return note(NOT_GITHUB);
        };
        let Some(branch) = repo.branch.as_deref() else {
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

    /// The workspace facet's answer: the folder whole, its names redacted
    /// (or left out where secrets can't be hidden), and its PR.
    fn workspace_said(&self, ws: &Workspace, pr: &Option<PullSeen>) -> Value {
        let r = match ws {
            Workspace::Nothing => return json!({ "note": NO_REPOSITORY }),
            Workspace::Unread => return json!({ "note": UNREAD }),
            Workspace::Repo(r) => r,
        };
        let named = self.desk.redact("").is_some();
        let redact = |t: &str| self.desk.redact(t).unwrap_or_default();
        let pull = pr.as_ref().and_then(|p| p.pull.as_ref());
        json!({
            "branch": r.branch,
            "head": r.head.as_deref().map(|h| h.chars().take(12).collect::<String>()),
            "upstream": r.upstream,
            "ahead": r.ahead_behind.map(|(a, _)| a),
            "behind": r.ahead_behind.map(|(_, b)| b),
            "unpushed": r.unpushed,
            "uncommitted": r.uncommitted.len(),
            "uncommitted_files": named.then(|| r.uncommitted.iter().take(UNCOMMITTED_NAMED).map(|f| redact(f)).collect::<Vec<_>>()),
            "recent": r.recent.iter().map(|c| json!({
                "commit": c.short,
                "at": c.at,
                "subject": named.then(|| redact(&c.subject)),
            })).collect::<Vec<_>>(),
            "origin": r.origin,
            "worktree": r.worktree,
            "left_behind": left(ws, pull),
            "pr": pull.map(|p| json!({
                "number": p.number,
                "state": if p.merged { "merged" } else if p.open { "open" } else { "closed" },
                "draft": p.draft,
                "checks": p.checks.as_deref().unwrap_or("none"),
                "failed": p.failed_names,
                "head_matches_local": pull_glance(p, r.head.as_deref()).head_matches_local,
                "url": p.url,
            })),
            "note": Some([pr.as_ref().and_then(|p| p.note.clone()), (!named).then(|| NAMES_HIDDEN.to_string())]
                .into_iter().flatten().collect::<Vec<_>>().join(" ")).filter(|n| !n.is_empty()),
        })
    }
}

/// What closing `ws`'s session would leave behind, in closing's words: a
/// merged pull request's head counts as pushed.
fn left(ws: &Workspace, pull: Option<&Pull>) -> Option<String> {
    let merged: Vec<String> = pull
        .filter(|p| p.merged)
        .and_then(|p| p.head.clone())
        .into_iter()
        .collect();
    workspace::held(&ws.folder(), &merged)
}

/// `ws` and its pull request as a glance says them.
fn workspace_glance(ws: &Workspace, pr: &Option<PullSeen>) -> Option<WorkspaceGlance> {
    let pull = pr.as_ref().and_then(|p| p.pull.as_ref());
    let left = left(ws, pull);
    Some(match ws {
        Workspace::Nothing => return None,
        Workspace::Unread => WorkspaceGlance {
            left,
            note: Some(UNREAD.into()),
            ..WorkspaceGlance::default()
        },
        Workspace::Repo(Repo {
            branch,
            head,
            ahead_behind,
            unpushed,
            uncommitted,
            ..
        }) => WorkspaceGlance {
            branch: branch.clone(),
            unpushed: *unpushed,
            uncommitted: uncommitted.len(),
            behind: ahead_behind.map(|(_, b)| b),
            left,
            pr: pull.map(|p| pull_glance(p, head.as_deref())),
            note: pr.as_ref().and_then(|p| p.note.clone()),
        },
    })
}

/// `v` as JSON, fenced as what came `from` the session.
fn fence(v: &impl serde::Serialize, from: &str) -> String {
    reported(&serde_json::to_string(v).unwrap_or_default(), from)
}
