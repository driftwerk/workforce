//! `crew.watch`, `crew.watches`: the waits a boss starts itself, as an
//! orchestrator starts a background watcher (designs/boss-as-claude-code.md
//! §3.3). A pull request until its checks are done (or it merges or
//! closes), a CI run until it completes, or a time: Workforce polls each,
//! as the person who owns it, and its end is a notice in the boss's own
//! conversation (`super::orchestrator`), told once, across restarts.
//!
//! - **The person's GitHub, read only.** The reads are `crew.github_*`'s
//!   ([`super::github::read`]), with the person's own key: nothing is
//!   written. Not connected, the watch ends at once saying so.
//! - **Bounded.** At most [`MAX_OPEN`] open for one boss and person, each
//!   for at most [`MAX_HOURS`] (24 by default), polled at most once a
//!   crew minute, backing off after a failed read; a watch on a plan ends
//!   with its plan.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_desk::boss_watch::{
    NewWatch, Opened, Watch, WatchKind, DEFAULT_HOURS, MAX_HOURS, MAX_OPEN,
};
use workforce_desk::model::SessionView;

use super::orchestrator::Notice;
use super::tools::Bound;
use super::watcher::reported;
use super::CrewHub;

pub(super) const WATCH: &str = "crew.watch";
pub(super) const WATCHES: &str = "crew.watches";

/// After a failed read, how many crew minutes until the next.
const BACKOFF_MINUTES: u32 = 5;

pub(super) const WATCH_DESCRIPTION: &str = "Start a background wait that wakes you when it \
    ends, as you'd run `gh run watch` in the background: a pull request (repo, number) until \
    its checks are done, or with until_merged until it merges or closes; a CI run (repo, \
    run_id) until it completes; or a time (minutes from now). Desk polls it on GitHub as the \
    person and tells you, in this conversation, how it ended, with your note. Use it whenever \
    work waits on CI, a review or a time, instead of waiting to be told. hours is how long it \
    may wait (24 by default, at most 72).";

pub(super) const WATCHES_DESCRIPTION: &str = "Your open waits (crew.watch), each with its id, \
    what it waits on and your note; with end (a watch id), ends that one.";

pub(super) fn schema(name: &str) -> Value {
    match name {
        WATCH => json!({ "type": "object", "properties": {
            "repo": { "type": "string", "description": "owner/name, for a pull request or a run" },
            "number": { "type": "integer", "description": "A pull request's number" },
            "until_merged": { "type": "boolean", "description": "Wait until the pull request merges or closes, not only until its checks are done" },
            "run_id": { "type": "integer", "description": "A workflow run's id" },
            "minutes": { "type": "integer", "description": "A time: this many minutes from now" },
            "note": { "type": "string", "description": "Why you wait and what you'll do when it ends: told back to you then" },
            "session_id": { "type": "string", "description": "The session the wait is for, if one" },
            "plan_id": { "type": "string", "description": "The plan the wait is for, if one: it ends with the plan" },
            "hours": { "type": "integer", "description": "How long it may wait: 24 by default, at most 72" }
        }, "required": ["note"] }),
        _ => json!({ "type": "object", "properties": {
            "end": { "type": "string", "description": "A watch id to end" }
        } }),
    }
}

/// What a watch asked for waits on.
#[derive(Debug, PartialEq)]
struct Asked {
    kind: WatchKind,
    repo: Option<String>,
    number: Option<i64>,
    run_id: Option<i64>,
    minutes: Option<i64>,
}

/// What a watch asked for waits on, read from the tool's input.
fn asked(input: &Value) -> Result<Asked, String> {
    let on = |kind, repo, number, run_id, minutes| Asked {
        kind,
        repo,
        number,
        run_id,
        minutes,
    };
    let int = |k: &str| input[k].as_i64().filter(|n| *n > 0);
    let repo = input["repo"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let repo = match repo {
        Some(r) => {
            // As GitHub names it, or nothing is read.
            let ok = r.split_once('/').is_some_and(|(o, n)| {
                [o, n].iter().all(|p| {
                    !p.is_empty()
                        && *p != "."
                        && *p != ".."
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                })
            });
            if !ok {
                return Err("Say the repository as owner/name (driftwerk/workforce).".into());
            }
            Some(r.to_string())
        }
        None => None,
    };
    match (int("number"), int("run_id"), int("minutes")) {
        (Some(n), None, None) => match repo {
            Some(r) => Ok(on(WatchKind::Pr, Some(r), Some(n), None, None)),
            None => Err("Say the pull request's repository (repo, owner/name).".into()),
        },
        (None, Some(run), None) => match repo {
            Some(r) => Ok(on(WatchKind::Run, Some(r), None, Some(run), None)),
            None => Err("Say the run's repository (repo, owner/name).".into()),
        },
        (None, None, Some(m)) => Ok(on(WatchKind::Time, None, None, None, Some(m))),
        _ => Err("Say one thing to wait on: a pull request (repo, number), a CI run (repo, run_id) or a time (minutes).".into()),
    }
}

/// A watch said for the boss.
fn said(w: &Watch, tz: chrono_tz::Tz) -> Value {
    json!({
        "watch_id": w.id,
        "waits_on": w.what(),
        "since": w.created_at.with_timezone(&tz).format("%H:%M").to_string(),
        "until": w.expires_at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string(),
        "note": w.note,
    })
}

impl CrewHub {
    /// `crew.watch` and `crew.watches`, for the boss `b`.
    pub(super) async fn watch_tool(
        &self,
        name: &str,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let person = workforce_agent::runner::CURRENT_USER_ID
            .try_with(Clone::clone)
            .ok()
            .filter(|p| !p.is_empty() && p != workforce_agent::runner::SYSTEM_USER_ID)
            .or_else(|| self.person(Some(&b.service)))
            .ok_or("No one has a Desk conversation with you yet, so there's no one to wait for.")?;
        let tz = self.desk.timezone();
        let store = self.desk.store();
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %format!("{e:#}"), "a boss's watches couldn't be read or kept");
            "Desk couldn't keep the watch just now; try again in a moment.".to_string()
        };
        if name == WATCHES {
            if let Some(end) = input["end"].as_str() {
                let id: Uuid = end
                    .trim()
                    .parse()
                    .map_err(|_| "Say which watch: its watch_id.")?;
                return match store
                    .watch_cancel(boss, &person, id)
                    .await
                    .map_err(internal)?
                {
                    true => Ok(json!({ "ended": id })),
                    false => {
                        Err("That isn't one of your open watches; crew.watches lists them.".into())
                    }
                };
            }
            let open = store.watches_open(boss, &person).await.map_err(internal)?;
            return Ok(json!({ "watches": open.iter().map(|w| said(w, tz)).collect::<Vec<_>>() }));
        }
        let Asked {
            kind,
            repo,
            number,
            run_id,
            minutes,
        } = asked(input)?;
        let note = input["note"].as_str().map(str::trim).unwrap_or_default();
        if note.is_empty() {
            return Err("Say in note why you wait and what you'll do when it ends.".into());
        }
        let session = match input["session_id"].as_str() {
            Some(s) => {
                let id: Uuid = s
                    .trim()
                    .parse()
                    .map_err(|_| "session_id is a session's id from crew.overview.")?;
                self.session_seen(id, &b.seen)?;
                Some(id)
            }
            None => None,
        };
        let plan = match input["plan_id"].as_str() {
            Some(p) => Some(
                p.trim()
                    .parse::<Uuid>()
                    .map_err(|_| "plan_id is a plan's id.")?,
            ),
            None => None,
        };
        let hours = input["hours"]
            .as_i64()
            .unwrap_or(DEFAULT_HOURS)
            .clamp(1, MAX_HOURS);
        let now = Utc::now();
        let due_at = minutes.map(|m| now + self.crew_minutes(m));
        let opened = store
            .watch_open(&NewWatch {
                boss: boss.to_string(),
                person: person.clone(),
                kind,
                repo,
                number,
                run_id,
                until_merged: input["until_merged"].as_bool().unwrap_or(false),
                note: workforce_desk::crew::clip(note, 600),
                session,
                plan,
                due_at,
                expires_at: now + chrono::Duration::hours(hours),
            })
            .await
            .map_err(internal)?;
        match opened {
            Opened::Started(w) => Ok(json!({ "started": said(&w, tz), "note": "You're told here when it ends." })),
            Opened::Already(w) => Ok(json!({ "already": said(&w, tz), "note": "That wait is open already: you're told when it ends." })),
            Opened::Full(open) => Err(format!(
                "You have {MAX_OPEN} waits open, the most at once; end one first (crew.watches with end). Open: {}",
                open.iter().map(|w| format!("{} ({})", w.what(), w.id)).collect::<Vec<_>>().join("; ")
            )),
        }
    }

    /// `m` minutes as the crew counts them (shorter in tests).
    fn crew_minutes(&self, m: i64) -> chrono::Duration {
        let one =
            chrono::Duration::from_std(self.timing.minute).unwrap_or(chrono::Duration::minutes(1));
        one * i32::try_from(m.clamp(1, MAX_HOURS * 60)).unwrap_or(1)
    }

    /// `boss`'s open watches for `person`, as its notices turn reads them.
    pub(super) async fn watches_said(&self, boss: &str, person: &str) -> Option<String> {
        let open = self.desk.store().watches_open(boss, person).await.ok()?;
        if open.is_empty() {
            return None;
        }
        let tz = self.desk.timezone();
        let lines: Vec<String> = open
            .iter()
            .map(|w| {
                format!(
                    "- {} (watch_id {}, since {}): {}",
                    w.what(),
                    w.id,
                    w.created_at.with_timezone(&tz).format("%H:%M"),
                    workforce_desk::crew::one_line(&w.note, 200)
                )
            })
            .collect();
        Some(format!(
            "Your open waits (you're told here when each ends):\n{}",
            reported(&lines.join("\n"), "your own watch notes")
        ))
    }

    /// The watches whose end a turn has now told: no longer in flight.
    pub(super) fn watches_told(&self, ids: &[Uuid]) {
        let mut flying = self.watches_in_flight.lock();
        for id in ids {
            flying.remove(id);
        }
    }

    /// One look at every watch: those that ended told to their boss's
    /// conversation, once.
    pub(super) async fn poll_watches(self: &Arc<Self>) {
        let store = self.desk.store();
        let now = Utc::now();
        match store.watches_open_all().await {
            Ok(open) => {
                for w in open {
                    if let Some(outcome) = self.look_at_watch(&w, now).await {
                        match store.watch_end(w.id, &outcome.0).await {
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %format!("{e:#}"), watch = %w.id, "an ended watch wasn't kept as ended")
                            }
                        }
                        // What it read, kept for the notice; told below.
                        self.watch_reads.lock().insert(w.id, outcome.1);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "the watches couldn't be read"),
        }
        let ended = match store.watches_undelivered().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the ended watches couldn't be read");
                return;
            }
        };
        for w in ended {
            if !self.watches_in_flight.lock().insert(w.id) {
                continue;
            }
            let read = self.watch_reads.lock().remove(&w.id).flatten();
            let text = watch_notice(&w, read.as_ref());
            let mut notice = Notice::new(text);
            notice.watch = Some(w.id);
            self.notify(&w.boss, &w.person, notice);
        }
    }

    /// How watch `w` ended, if it did: in words, and what was read.
    async fn look_at_watch(
        &self,
        w: &Watch,
        now: DateTime<Utc>,
    ) -> Option<(String, Option<Value>)> {
        if now >= w.expires_at {
            return Some((
                format!(
                    "stopped waiting after {} hours: it hadn't ended",
                    (w.expires_at - w.created_at).num_hours()
                ),
                None,
            ));
        }
        if let Some(plan) = w.plan {
            use workforce_desk::plan::PlanState;
            if let Ok(Some(p)) = self.desk.store().plan(plan).await {
                if matches!(p.state, PlanState::Done | PlanState::Dropped) {
                    return Some(("its plan ended, so it stopped waiting".into(), None));
                }
            }
        }
        if w.kind == WatchKind::Time {
            return w
                .due_at
                .is_none_or(|at| now >= at)
                .then(|| ("the time came".into(), None));
        }
        // Backing off after a failed read.
        if w.failures > 0 {
            let back = chrono::Duration::from_std(self.timing.minute * BACKOFF_MINUTES)
                .unwrap_or_default();
            if w.polled_at.is_some_and(|at| now - at < back) {
                return None;
            }
        }
        let Some((service, _)) = self.overseer_named(&w.boss) else {
            return Some((
                "its boss no longer oversees, so it stopped waiting".into(),
                None,
            ));
        };
        let (name, input) = match w.kind {
            WatchKind::Pr => (
                super::github::PR,
                json!({ "repo": w.repo, "number": w.number }),
            ),
            _ => (
                super::github::CI,
                json!({ "repo": w.repo, "run_id": w.run_id }),
            ),
        };
        let read = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                w.person.clone(),
                super::github::read(
                    service.vault.clone(),
                    service.global_vault.clone(),
                    &service.config,
                    &service.tools,
                    name,
                    &input,
                ),
            )
            .await;
        let got = match read {
            Ok(v) => v,
            Err(e) if e == super::github::NOT_CONNECTED => {
                return Some(("GitHub isn't connected for the person, so it can't be watched: tell them \"Connect GitHub to let me check.\"".into(), None));
            }
            Err(e) => {
                tracing::info!(watch = %w.id, error = %e, "a watch's read failed; it backs off");
                if let Err(e) = self.desk.store().watch_polled(w.id, true).await {
                    tracing::warn!(error = %format!("{e:#}"), "a watch's failed read wasn't noted");
                }
                return None;
            }
        };
        if let Err(e) = self.desk.store().watch_polled(w.id, false).await {
            tracing::warn!(error = %format!("{e:#}"), "a watch's read wasn't noted");
        }
        ended(w, &got).map(|o| (o, Some(got)))
    }
}

/// How `w` ended, given what GitHub says now (`got`), if it did.
pub(super) fn ended(w: &Watch, got: &Value) -> Option<String> {
    match w.kind {
        WatchKind::Pr => {
            if got["merged"].as_bool() == Some(true) {
                return Some("it merged".into());
            }
            if got["state"].as_str() == Some("closed") {
                return Some("it was closed without merging".into());
            }
            if w.until_merged {
                return None;
            }
            let c = &got["checks"];
            let total = c["total"].as_u64().unwrap_or(0);
            let pending = c["pending"].as_u64().unwrap_or(0);
            (total > 0 && pending == 0).then(|| {
                format!(
                    "its checks are done: {} ({} passed, {} failed)",
                    c["overall"].as_str().unwrap_or("?"),
                    c["success"].as_u64().unwrap_or(0),
                    c["failure"].as_u64().unwrap_or(0),
                )
            })
        }
        WatchKind::Run => {
            let run = &got["run"];
            if run["status"].as_str() != Some("completed") {
                return None;
            }
            let conclusion = run["conclusion"].as_str().unwrap_or("?");
            let mut said = format!("the run completed: {conclusion}");
            if conclusion == "failure" && lost_runner(got) {
                said.push_str(&format!(
                    ". A failed job has no failed step: likely a lost runner, so rerun the failed jobs (gh run rerun {} --failed) rather than fixing code",
                    w.run_id.unwrap_or_default()
                ));
            }
            Some(said)
        }
        WatchKind::Time => None,
    }
}

/// Whether a failed run's read shows a job that failed with no failed
/// step: a runner lost, not the code.
fn lost_runner(got: &Value) -> bool {
    // The jobs are fenced as data for the boss; their shape is read here.
    let named = got["named"].to_string();
    named.contains("\\\"failed_steps\\\":[]") || named.contains("\"failed_steps\":[]")
}

/// The notice of `w`'s end, with what was read.
fn watch_notice(w: &Watch, read: Option<&Value>) -> String {
    let mut text = format!(
        "Your watch on {} (watch_id {}) ended: {}. Your note when you started it: {}",
        w.what(),
        w.id,
        w.outcome.as_deref().unwrap_or("it ended"),
        reported(&w.note, "your own watch note"),
    );
    if let Some(read) = read {
        text.push_str(&format!(
            " GitHub now: {}",
            reported(&read.to_string(), "GitHub")
        ));
    }
    if let Some(s) = w.session {
        text.push_str(&format!(" (for session_id {s})"));
    }
    text
}

impl CrewHub {
    /// What the boss is told of `view`'s turn ending.
    pub(super) async fn turn_end_notice(&self, view: &SessionView) -> String {
        let said = view
            .last_reply
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .map(|r| {
                reported(
                    &workforce_desk::crew::clip(r, 3000),
                    &format!("{}'s last reply", view.label()),
                )
            })
            .unwrap_or_else(|| "(it said nothing)".into());
        let line = format!(
            "{} (session_id {}) finished its turn, for: {}. It said: {said}",
            view.label(),
            view.id,
            workforce_desk::crew::one_line(&view.goal, 200),
        );
        self.with_doing(view.id, line).await
    }
}

/// Looks at every watch each crew minute, and starts what waits on notices.
pub(super) async fn watch_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(crew.timing.minute);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        crew.poll_watches().await;
        crew.kick_notices();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(kind: WatchKind, until_merged: bool) -> Watch {
        let now = Utc::now();
        Watch {
            id: Uuid::new_v4(),
            boss: "desk".into(),
            person: "p".into(),
            kind,
            repo: Some("acme/web".into()),
            number: Some(5),
            run_id: Some(77),
            until_merged,
            note: "merge it".into(),
            session: None,
            plan: None,
            created_at: now,
            due_at: None,
            expires_at: now + chrono::Duration::hours(24),
            polled_at: None,
            failures: 0,
            ended_at: None,
            outcome: None,
            delivered_at: None,
        }
    }

    #[test]
    fn a_pull_request_ends_when_its_checks_are_done_or_it_merges() {
        let pr = watch(WatchKind::Pr, false);
        let pending = json!({ "state": "open", "merged": false, "checks": { "overall": "pending", "total": 3, "pending": 1, "success": 2, "failure": 0 } });
        assert_eq!(ended(&pr, &pending), None);
        let none_yet =
            json!({ "state": "open", "merged": false, "checks": { "overall": "none yet" } });
        assert_eq!(ended(&pr, &none_yet), None, "no checks yet is no end");
        let green = json!({ "state": "open", "merged": false, "checks": { "overall": "success", "total": 3, "pending": 0, "success": 3, "failure": 0 } });
        assert_eq!(
            ended(&pr, &green).as_deref(),
            Some("its checks are done: success (3 passed, 0 failed)")
        );
        let until = watch(WatchKind::Pr, true);
        assert_eq!(ended(&until, &green), None, "until it merges");
        let merged = json!({ "state": "closed", "merged": true });
        assert_eq!(ended(&until, &merged).as_deref(), Some("it merged"));
        let closed = json!({ "state": "closed", "merged": false });
        assert!(ended(&until, &closed)
            .unwrap()
            .contains("closed without merging"));
    }

    #[test]
    fn a_run_ends_when_it_completes_and_a_lost_runner_is_said() {
        let run = watch(WatchKind::Run, false);
        assert_eq!(
            ended(&run, &json!({ "run": { "status": "in_progress" } })),
            None
        );
        let green =
            json!({ "run": { "status": "completed", "conclusion": "success" }, "named": "" });
        assert_eq!(
            ended(&run, &green).as_deref(),
            Some("the run completed: success")
        );
        let lost = json!({ "run": { "status": "completed", "conclusion": "failure" },
            "named": reported(&json!({ "failed_jobs": [{ "name": "test", "failed_steps": [] }] }).to_string(), "GitHub") });
        let said = ended(&run, &lost).unwrap();
        assert!(said.contains("gh run rerun 77 --failed"), "{said}");
        let real = json!({ "run": { "status": "completed", "conclusion": "failure" },
            "named": reported(&json!({ "failed_jobs": [{ "name": "test", "failed_steps": ["cargo test"] }] }).to_string(), "GitHub") });
        assert!(!ended(&run, &real).unwrap().contains("rerun"));
    }

    #[test]
    fn one_thing_to_wait_on_is_asked_for() {
        assert!(asked(&json!({ "repo": "acme/web", "number": 5 })).is_ok());
        assert!(asked(&json!({ "number": 5 })).is_err(), "no repo");
        assert!(
            asked(&json!({ "repo": "acme/web", "number": 5, "minutes": 3 })).is_err(),
            "two"
        );
        assert!(
            asked(&json!({ "repo": "../x", "run_id": 5 })).is_err(),
            "not a repo"
        );
        assert!(matches!(
            asked(&json!({ "minutes": 10 })),
            Ok(Asked {
                kind: WatchKind::Time,
                ..
            })
        ));
    }
}
