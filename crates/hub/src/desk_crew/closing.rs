//! The boss closes the sessions it runs once their work is done
//! (`[oversight.work] close_finished`, on unless the person turns it off),
//! so they leave the grid and the sidebar instead of hanging around: each
//! is closed in Desk (`Desk::end`) and removed from it as the person's
//! Remove does (`Desk::remove_closed`), off the closed list too, its
//! conversation and folder kept: never deleted.
//!
//! - **Done** is the work finished: its pull request merged on GitHub (as
//!   the person reads it, [`super::github`]) with none still open, or every
//!   ticket it was given done, or its boss judged its last turn done at Act
//!   fully ([`super::idle`]), or it sat a day with nothing open.
//! - **Never the person's.** Only sessions in a boss's charge that it
//!   started, or (at Act fully) that the person handed it: never one it
//!   took on by itself, one the person typed into lately or holds the
//!   wheel of, one in a Desk terminal or another terminal, or one asking
//!   or waiting on anything.
//! - **Nothing lost.** Its folder is checked first: uncommitted files or
//!   commits on no remote (one whose merged pull request's head is its
//!   HEAD is pushed) keep it open, and the boss says so once.
//! - **Levels.** At Act fully it closes; at Act it closes the ones it
//!   started and suggests the rest; below, it suggests. Each close is a
//!   decision whose Undo brings it back and resumes it, and the boss's next
//!   update says them in one line ([`workforce_desk::update::closed_line`]).
//! - **Asked.** The person asking it to close what is finished, the boss
//!   closes and removes with `crew.close_session` ([`TOOL`]): at Act fully
//!   one it may end runs, anything else is a suggestion, and every close
//!   of a boss's, swept or asked, is held by [`CrewHub::close_held`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::account::RefKind;
use workforce_desk::board::TicketState;
use workforce_desk::charge::How;
use workforce_desk::crew::{By, MentionedSuggestion};
use workforce_desk::model::{Origin, SessionState, SessionView};
use workforce_desk::update::{ClosedKind, ClosedSession};

use super::actions::Action;
use super::{name_of, AgentService, CrewHub};

/// The action's tool name: a suggestion to close a session holds it.
pub(crate) const TOOL: &str = "crew.close_session";

/// How often, in the crew's minutes, the sessions are looked at.
const EVERY_MINUTES: u32 = 5;
/// How long a session rests before it is closed: what it did is seen first.
pub(crate) const SETTLE: chrono::Duration = chrono::Duration::minutes(10);
/// How long a session sits with nothing open before it is closed as done.
const IDLE_HOURS: i64 = 24;
/// How long the person's last input keeps a session theirs here.
const HANDS_OFF_HOURS: i64 = 2;
/// How long the person's last input keeps a session theirs from any close
/// of its boss's, asked for or not: they are in it.
const PERSON_NOW_MINUTES: i64 = 15;
/// How long an open pull request's state is believed before it is read
/// again: GitHub's rate limit is everyone's, but a session waiting on its
/// CI is told within minutes ([`super::plan_watch`]).
const PULL_AGAIN: Duration = Duration::from_secs(5 * 60);
/// How long a git command in a session's folder may take.
const GIT_WITHIN: Duration = Duration::from_secs(10);
/// The most pull requests one session's account names that are read.
const PULLS_MAX: usize = 5;

/// Why a session's work is done, as the person reads it after a dash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finished {
    Merged(u64),
    Ticket,
    Said(String),
    Idle,
}

impl Finished {
    fn why(&self) -> String {
        match self {
            Self::Merged(n) => format!("Its PR #{n} merged."),
            Self::Ticket => "Its ticket is done.".into(),
            Self::Said(why) => {
                let why = workforce_desk::crew::one_line(why, 160);
                match why.trim() {
                    "" => "It finished its work.".into(),
                    w => format!("It finished its work: {}", w.trim_end_matches('.')) + ".",
                }
            }
            Self::Idle => "It sat a day with nothing left to do.".into(),
        }
    }
}

/// What a session's folder holds that closing it would leave behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Folder {
    /// Nothing to lose: no repository there (or no folder at all).
    Nothing,
    /// A repository with everything committed: its HEAD, and how many of
    /// its commits are on no remote.
    Clean { head: Option<String>, unpushed: u32 },
    /// Uncommitted files, this many.
    Uncommitted(usize),
    /// It couldn't be read: kept open.
    Unread,
}

/// A pull request as GitHub last said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pull {
    pub open: bool,
    pub merged: bool,
    pub head: Option<String>,
    /// Its checks on its head overall: `success`, `failure`, `pending`,
    /// or none when none reported or they couldn't be read.
    pub checks: Option<String>,
    /// How many of its checks failed.
    pub failed: u64,
    /// When it merged, as GitHub says.
    pub merged_at: Option<String>,
}

/// What the closer keeps between looks.
#[derive(Default)]
pub(crate) struct Closing {
    /// The sessions their Act fully boss judged done, at which prompt, and
    /// why.
    said_done: HashMap<Uuid, (u64, String)>,
    /// Pull requests as GitHub last said, by repository and number, and
    /// when.
    pulls: HashMap<(String, u64), (Pull, Instant)>,
    /// What was said of each session already, never said again: its
    /// suggestion, or why it was left open.
    told: HashSet<(Uuid, String)>,
    /// What each boss's next update says of the finished sessions it would
    /// close or left open, and when each was noted.
    notes: HashMap<String, Vec<(DateTime<Utc>, ClosedKind, ClosedSession)>>,
}

/// What the boss does with a finished session, by how it came to run it
/// and its level now; none, nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Do {
    Close,
    Suggest,
}

/// The table: one it started is closed at Act and Act fully, one handed
/// to it only at Act fully, and suggested where it isn't closed; one it
/// took on by itself is the person's own, never closed; at Watch it only
/// looks.
pub(crate) fn what_to_do(level: Level, how: How) -> Option<Do> {
    match (how, level) {
        (_, Level::Watch) | (How::Taken, _) => None,
        (How::Started, Level::Act | Level::Full) | (How::Handed, Level::Full) => Some(Do::Close),
        (How::Started | How::Handed, _) => Some(Do::Suggest),
    }
}

/// Whether a session in `v`'s state may be closed now: at rest, started
/// by Desk and not in a terminal, nothing asked or waiting, rested
/// `settle` ([`SETTLE`]).
pub(crate) fn at_rest(v: &SessionView, now: DateTime<Utc>, settle: chrono::Duration) -> bool {
    matches!(
        v.state,
        SessionState::Done | SessionState::Idle | SessionState::Stopped | SessionState::Interrupted
    ) && v.origin == Origin::Desk
        && v.terminal_id.is_none()
        && v.pending.is_none()
        && v.asked.is_none()
        && now - v.updated_at >= settle
}

/// Why a session's work is done, from what is known of it: any ticket
/// still open, or pull request still open, is work left; then a merged
/// pull request, its tickets all done, or (when every pull request was
/// read) its boss's word or a day idle.
pub(crate) fn finished(
    tickets: &[TicketState],
    pulls: &[(u64, Option<Pull>)],
    said: Option<&str>,
    idle: bool,
) -> Option<Finished> {
    let open_ticket = tickets
        .iter()
        .any(|t| !matches!(t, TicketState::Done | TicketState::Dropped));
    if open_ticket
        || pulls
            .iter()
            .any(|(_, p)| p.as_ref().is_some_and(|p| p.open))
    {
        return None;
    }
    if let Some((n, _)) = pulls
        .iter()
        .find(|(_, p)| p.as_ref().is_some_and(|p| p.merged))
    {
        return Some(Finished::Merged(*n));
    }
    if tickets.contains(&TicketState::Done) {
        return Some(Finished::Ticket);
    }
    // A pull request GitHub couldn't say may be open still: neither its
    // boss's word nor a quiet day closes it.
    if pulls.iter().any(|(_, p)| p.is_none()) {
        return None;
    }
    if let Some(why) = said {
        return Some(Finished::Said(why.to_string()));
    }
    idle.then_some(Finished::Idle)
}

/// Why `folder` keeps a session open, in a few words; none when nothing
/// would be left behind. Commits on no remote count as pushed when a
/// merged pull request's head is its HEAD (its branch deleted on merge).
pub(crate) fn held(folder: &Folder, merged_heads: &[String]) -> Option<String> {
    match folder {
        Folder::Nothing => None,
        Folder::Clean { unpushed: 0, .. } => None,
        Folder::Clean { head: Some(h), .. } if merged_heads.iter().any(|m| m == h) => None,
        Folder::Clean { unpushed: 1, .. } => Some("1 commit not pushed".into()),
        Folder::Clean { unpushed, .. } => Some(format!("{unpushed} commits not pushed")),
        Folder::Uncommitted(1) => Some("1 uncommitted file".into()),
        Folder::Uncommitted(n) => Some(format!("{n} uncommitted files")),
        Folder::Unread => Some("its folder couldn't be checked".into()),
    }
}

/// `owner/name` of a GitHub remote's URL (`https://github.com/o/r.git`,
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`); none for
/// another host.
pub(crate) fn github_repo(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = u
        .strip_prefix("git@github.com:")
        .or_else(|| u.strip_prefix("ssh://git@github.com/"))
        .or_else(|| u.strip_prefix("https://github.com/"))
        .or_else(|| u.strip_prefix("http://github.com/"))
        .or_else(|| u.strip_prefix("git://github.com/"))?;
    let mut parts = rest.trim_end_matches('/').splitn(3, '/');
    let owner = parts.next()?;
    let name = parts.next()?.trim_end_matches(".git");
    let ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// The repository and number of a pull request an account names: by its
/// URL, as `owner/name#n`, or as `#n` of the folder's own repository
/// (`origin`).
pub(crate) fn pull_ref(id: &str, url: Option<&str>, origin: Option<&str>) -> Option<(String, u64)> {
    if let Some(u) = url {
        if let Some((repo, n)) = u.split_once("/pull/") {
            let n: u64 = n
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            return github_repo(repo).map(|r| (r, n));
        }
    }
    let id = id.trim();
    let (repo, n) = match id.split_once('#') {
        Some((r, n)) if !r.trim().is_empty() => {
            (github_repo(&format!("https://github.com/{}", r.trim()))?, n)
        }
        Some((_, n)) => (origin?.to_string(), n),
        None => (origin?.to_string(), id),
    };
    n.trim().parse().ok().map(|n| (repo, n))
}

/// Runs git in `cwd`, hardened (no hook or fsmonitor runs), its trimmed
/// output; `Err(true)` when `cwd` is no repository, `Err(false)` when it
/// couldn't be run.
async fn git(cwd: &Path, args: &[&str]) -> Result<String, bool> {
    let mut cmd = crate::git_hardening::hardened_git();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(GIT_WITHIN, cmd.output()).await {
        Ok(Ok(out)) => out,
        _ => return Err(false),
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    Err(err.contains("not a git repository"))
}

/// What `cwd` holds that closing its session would leave behind. Nothing is
/// fetched: what this computer knows of its remotes is what counts.
pub(crate) async fn folder(cwd: &str) -> Folder {
    let dir = Path::new(cwd);
    if cwd.trim().is_empty() || !dir.is_dir() {
        return Folder::Nothing;
    }
    match git(dir, &["rev-parse", "--is-inside-work-tree"]).await {
        Ok(t) if t == "true" => {}
        // A bare repository or `.git` itself: nothing of the session's.
        Ok(_) | Err(true) => return Folder::Nothing,
        Err(false) => return Folder::Unread,
    }
    let Ok(status) = git(dir, &["status", "--porcelain", "--untracked-files=normal"]).await else {
        return Folder::Unread;
    };
    // Finder's own files are nobody's work (the person's global ignore
    // isn't read by a hardened git).
    let changed = status
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.trim_end().ends_with(".DS_Store"))
        .count();
    if changed > 0 {
        return Folder::Uncommitted(changed);
    }
    // No commit yet: nothing committed to lose.
    let Ok(head) = git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).await else {
        return Folder::Clean {
            head: None,
            unpushed: 0,
        };
    };
    match git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"]).await {
        Ok(n) => match n.parse() {
            Ok(unpushed) => Folder::Clean {
                head: Some(head),
                unpushed,
            },
            Err(_) => Folder::Unread,
        },
        Err(_) => Folder::Unread,
    }
}

/// `origin`'s GitHub repository of the folder `cwd`, when it has one.
async fn origin_of(cwd: &str) -> Option<String> {
    let dir = Path::new(cwd);
    if !dir.is_dir() {
        return None;
    }
    // As configured: a repository's own `insteadOf` doesn't move it.
    git(dir, &["config", "--get", "remote.origin.url"])
        .await
        .ok()
        .and_then(|u| github_repo(&u))
}

/// Whether closing `v` and removing it loses nothing now: at rest, asking
/// nothing, and nothing uncommitted or unpushed in its folder. Read for a
/// close the person said yes to, which no boss's gate holds.
pub(crate) async fn nothing_to_lose(v: &SessionView) -> bool {
    v.pending.is_none()
        && v.asked.is_none()
        && !matches!(
            v.state,
            SessionState::Starting
                | SessionState::Working
                | SessionState::Blocked
                | SessionState::Asking
        )
        && held(&folder(&v.cwd).await, &[]).is_none()
}

/// Looks at the bosses' finished sessions every few minutes.
pub(super) async fn closing_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let mut every = tokio::time::interval(crew.timing.minute * EVERY_MINUTES);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = every.tick() => crew.look_at_finished_sessions().await,
        }
    }
}

impl CrewHub {
    /// Notes that `boss` judged `view`'s last turn done (at Act fully): the
    /// session said it is finished.
    pub(super) fn said_done(&self, view: &SessionView, why: &str) {
        self.closing
            .lock()
            .said_done
            .insert(view.id, (view.prompts, why.to_string()));
    }

    /// The notes on finished sessions `boss`'s next update says, noted by
    /// `cutoff`, by kind.
    pub(super) fn closing_notes(
        &self,
        boss: &str,
        cutoff: DateTime<Utc>,
    ) -> Vec<(ClosedKind, ClosedSession)> {
        self.closing
            .lock()
            .notes
            .get(boss)
            .map(|n| {
                n.iter()
                    .filter(|(at, _, _)| *at <= cutoff)
                    .map(|(_, k, s)| (*k, s.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `boss`'s update said what was noted by `cutoff`: let go.
    pub(super) fn closing_told(&self, boss: &str, cutoff: DateTime<Utc>) {
        if let Some(n) = self.closing.lock().notes.get_mut(boss) {
            n.retain(|(at, _, _)| *at > cutoff);
        }
    }

    /// Whether `boss` has notes on finished sessions for its next update.
    pub(super) fn has_closing_notes(&self, boss: &str) -> bool {
        self.closing
            .lock()
            .notes
            .get(boss)
            .is_some_and(|n| !n.is_empty())
    }

    /// Looks once at every boss's finished sessions: closes, suggests or
    /// says what keeps each open, as its level and settings say.
    pub(crate) async fn look_at_finished_sessions(&self) {
        if !self.desk.charges_loaded() {
            return;
        }
        // One look at a time: two would close a session twice.
        let Ok(_one) = self.closing_now.try_lock() else {
            return;
        };
        let views = self.desk.list();
        let now = Utc::now();
        let settle = *self.close_settle.lock();
        {
            // Only what is still there and still at that turn is kept.
            let open: HashMap<Uuid, u64> = views
                .iter()
                .filter(|v| v.state != SessionState::Closed)
                .map(|v| (v.id, v.prompts))
                .collect();
            let mut c = self.closing.lock();
            c.said_done
                .retain(|id, (prompts, _)| open.get(id) == Some(prompts));
            c.told.retain(|(id, _)| open.contains_key(id));
        }
        for (service, o) in self.overseers() {
            if !o.work.close_finished {
                continue;
            }
            let boss = service.agent_id.as_str().to_string();
            let level = self.level_now(&boss, &o).0;
            let mine: Vec<(&SessionView, Do, How)> = views
                .iter()
                .filter_map(|v| {
                    let c = self.desk.charge_of(v.id)?;
                    (c.live() && c.boss == boss && at_rest(v, now, settle))
                        .then(|| what_to_do(level, c.how))
                        .flatten()
                        .map(|d| (v, d, c.how))
                })
                .collect();
            if mine.is_empty() {
                continue;
            }
            let asked: HashSet<Uuid> = match self.desk.store().items_not_closed(Some(&boss)).await {
                Ok(items) => items.iter().filter_map(|i| i.session).collect(),
                // What waits on the person isn't known: nothing is closed.
                Err(e) => {
                    tracing::info!(%boss, error = %format!("{e:#}"), "the open items couldn't be read; no session is closed this time");
                    continue;
                }
            };
            for (v, what, how) in mine {
                if asked.contains(&v.id) {
                    continue;
                }
                self.look_at_done(&service, &o, v, what, how).await;
            }
        }
    }

    /// Looks at `v`, one `service` runs, and does `what` when its work is
    /// done and nothing would be lost.
    async fn look_at_done(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        v: &SessionView,
        what: Do,
        how: How,
    ) {
        let now = Utc::now();
        let boss = service.agent_id.as_str();
        // The person in it lately, or at its wheel: theirs.
        let hands_off = chrono::Duration::hours(HANDS_OFF_HOURS);
        if self.person_in_for(v.id, now - hands_off, self.answer_gap(o)) {
            return;
        }
        // Stopped, or Desk started again since: what the person typed in it
        // is no longer known, so it rests the whole while first.
        if !v.live && now - v.updated_at < hands_off {
            return;
        }
        // The person reopened it after a close: theirs to close.
        match self.desk.store().close_undone(v.id).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "whether a close of a session was undone couldn't be read; it isn't closed");
                return;
            }
        }
        let tickets: Vec<TicketState> = match self.desk.store().session_tickets(v.id).await {
            Ok(t) => t.into_iter().map(|t| t.state).collect(),
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "a session's tickets couldn't be read; it isn't closed");
                return;
            }
        };
        let said = {
            let c = self.closing.lock();
            c.said_done
                .get(&v.id)
                .filter(|(p, _)| *p == v.prompts)
                .map(|(_, w)| w.clone())
        };
        let a_day = chrono::Duration::hours(IDLE_HOURS);
        // Only one it started itself is closed for being quiet.
        let idle = how == How::Started
            && now - v.updated_at >= a_day
            && self
                .desk
                .person_input_at(v.id)
                .is_none_or(|at| now - at >= a_day);
        let pulls = self.pulls_of(service, v).await;
        let Some(why) = finished(&tickets, &pulls, said.as_deref(), idle) else {
            return;
        };
        let merged: Vec<String> = pulls
            .iter()
            .filter_map(|(_, p)| p.as_ref().filter(|p| p.merged))
            .filter_map(|p| p.head.clone())
            .collect();
        let name = v.label().to_string();
        let session = ClosedSession {
            session_id: v.id.to_string(),
            name: name.clone(),
            why: workforce_desk::decision::clause(&why.why()),
            decision_id: None,
            undo: None,
            suggestion_id: None,
        };
        if let Some(kept) = held(&folder(&v.cwd).await, &merged) {
            self.note_once(
                boss,
                v.id,
                "open",
                ClosedKind::KeptOpen,
                ClosedSession {
                    why: Some(kept),
                    ..session
                },
            );
            return;
        }
        // Looked at again just now: still where it was.
        let still = self.desk.view(v.id).is_ok_and(|now_v| {
            now_v.prompts == v.prompts && now_v.state == v.state && now_v.terminal_id.is_none()
        }) && !self.person_in_for(v.id, Utc::now() - hands_off, self.answer_gap(o));
        if !still {
            return;
        }
        let action = Action::Close { session: v.id };
        let by = By::manager(boss, &name_of(service), &why.why());
        match what {
            Do::Close => match self.perform(&action, by).await {
                Ok(done) => {
                    tracing::info!(session = %v.id, %boss, %done, "a boss closed a finished session");
                    self.closing.lock().said_done.remove(&v.id);
                }
                Err(refused) => {
                    tracing::info!(session = %v.id, %boss, %refused, "a finished session couldn't be closed");
                    // Still stopping, it closes once it has: not left open.
                    if self
                        .desk
                        .view(v.id)
                        .map_or(true, |now_v| now_v.state != v.state)
                    {
                        return;
                    }
                    self.note_once(
                        boss,
                        v.id,
                        "refused",
                        ClosedKind::KeptOpen,
                        ClosedSession {
                            why: Some(workforce_desk::crew::words(
                                &refused,
                                workforce_desk::decision::WHY_WORDS,
                            )),
                            ..session
                        },
                    );
                }
            },
            Do::Suggest => {
                if self.closing.lock().told.contains(&(v.id, "suggest".into()))
                    || self.close_suggested(v.id).await
                {
                    return;
                }
                let input = json!({ "session_id": v.id.to_string() });
                match self.suggest(TOOL, &input, &action, &by).await {
                    Ok(s) => {
                        self.quiet_made
                            .lock()
                            .entry(boss.to_string())
                            .or_default()
                            .push((
                                Utc::now(),
                                MentionedSuggestion {
                                    id: s.id.clone(),
                                    text: s.text.clone(),
                                    state: s.state,
                                },
                            ));
                        self.note_once(
                            boss,
                            v.id,
                            "suggest",
                            ClosedKind::Suggested,
                            ClosedSession {
                                suggestion_id: Some(s.id),
                                ..session
                            },
                        );
                    }
                    Err(e) => {
                        tracing::info!(session = %v.id, %boss, error = %e, "suggesting a finished session be closed failed")
                    }
                }
            }
        }
    }

    /// Why `boss` may not close `v` now, whichever way its close came (its
    /// sweep, or its own call when the person asked it to close what is
    /// finished): still at work, asking, waiting on the person, the
    /// person's now, or with work in its folder a close would leave behind.
    /// None: it may. Whether it is the boss's to close at all (it started
    /// it or was handed it) is Desk's to say, as it ends it.
    pub(super) async fn close_held(&self, v: &SessionView, boss: &str) -> Option<String> {
        if v.origin != Origin::Desk {
            return Some("It runs in another terminal, so it's the person's to close.".into());
        }
        if v.pending.is_some() || v.asked.is_some() || v.state == SessionState::Asking {
            return Some(
                "It's asking something: answer it, or ask the person, before it is closed.".into(),
            );
        }
        if matches!(
            v.state,
            SessionState::Starting | SessionState::Working | SessionState::Blocked
        ) {
            return Some("It's still working: it is closed only once its turn has ended.".into());
        }
        let Some((service, o)) = self.overseer_named(boss) else {
            return Some("No boss runs it now.".into());
        };
        // Rested first, as the closer's own: what it did is seen.
        if !at_rest(v, Utc::now(), *self.close_settle.lock()) {
            return Some(format!(
                "Its turn ended under {} minutes ago: it is closed once it has rested.",
                self.close_settle.lock().num_minutes().max(1)
            ));
        }
        let now = chrono::Duration::minutes(PERSON_NOW_MINUTES);
        if self.person_in_for(v.id, Utc::now() - now, self.answer_gap(&o)) {
            return Some("The person is in it now, so it's theirs to close.".into());
        }
        match self.desk.store().items_not_closed(Some(boss)).await {
            Ok(items) if items.iter().any(|i| i.session == Some(v.id)) => {
                return Some(
                    "It waits on the person for something: it stays open until that's answered."
                        .into(),
                )
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "the open items couldn't be read; the session isn't closed");
                return Some(
                    "What waits on the person couldn't be read, so it stays open for now.".into(),
                );
            }
        }
        // Work it was given still open is work left.
        match self.desk.store().session_tickets(v.id).await {
            Ok(t)
                if t.iter()
                    .any(|t| !matches!(t.state, TicketState::Done | TicketState::Dropped)) =>
            {
                return Some("Its ticket is still open: it stays open until that's done.".into())
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "a session's tickets couldn't be read; it isn't closed");
                return Some("Its tickets couldn't be read, so it stays open for now.".into());
            }
        }
        let pulls = self.pulls_of(&service, v).await;
        if let Some((n, _)) = pulls
            .iter()
            .find(|(_, p)| p.as_ref().is_some_and(|p| p.open))
        {
            return Some(format!(
                "Its PR #{n} is still open: it stays open until that's merged or closed."
            ));
        }
        let merged: Vec<String> = pulls
            .iter()
            .filter_map(|(_, p)| p.as_ref().filter(|p| p.merged))
            .filter_map(|p| p.head.clone())
            .collect();
        held(&folder(&v.cwd).await, &merged)
            .map(|kept| format!("It stays open ({kept}): nothing is closed that would lose work."))
    }

    /// Notes `s` for `boss`'s next update, once for `session` and `what`.
    fn note_once(&self, boss: &str, session: Uuid, what: &str, kind: ClosedKind, s: ClosedSession) {
        let mut c = self.closing.lock();
        if !c.told.insert((session, what.to_string())) {
            return;
        }
        c.notes
            .entry(boss.to_string())
            .or_default()
            .push((Utc::now(), kind, s));
    }

    /// Whether closing `session` was suggested already: open still, or
    /// answered (a dismissed one is the person's word to leave it).
    async fn close_suggested(&self, session: Uuid) -> bool {
        let id = session.to_string();
        self.desk
            .store()
            .suggestions(200)
            .await
            .unwrap_or_default()
            .iter()
            .any(|s| {
                s.action["tool"] == TOOL
                    && s.action["input"]["session_id"] == Value::String(id.clone())
            })
    }

    /// The pull requests `v`'s account names, each as GitHub says now
    /// (none where it couldn't be read).
    pub(super) async fn pulls_of(
        &self,
        service: &Arc<AgentService>,
        v: &SessionView,
    ) -> Vec<(u64, Option<Pull>)> {
        let account = match self.desk.session_account(v.id).await {
            Ok(Some(a)) => a,
            _ => return Vec::new(),
        };
        let refs: Vec<(String, Option<String>)> = account
            .done
            .iter()
            .flat_map(|d| &d.refs)
            .filter(|r| r.kind == RefKind::Pr)
            .map(|r| (r.id.clone(), r.url.clone()))
            .collect();
        if refs.is_empty() {
            return Vec::new();
        }
        let origin = origin_of(&v.cwd).await;
        let mut seen: Vec<(String, u64)> = Vec::new();
        for (id, url) in refs {
            if let Some(p) = pull_ref(&id, url.as_deref(), origin.as_deref()) {
                if !seen.contains(&p) {
                    seen.push(p);
                }
            }
        }
        let mut out = Vec::new();
        for (repo, n) in seen.into_iter().take(PULLS_MAX) {
            out.push((n, self.pull(service, &repo, n).await));
        }
        out
    }

    /// Takes `p` as what GitHub says of pull request `n` of `repo` now.
    #[cfg(test)]
    pub(crate) fn pull_seen(&self, repo: &str, n: u64, p: Pull) {
        self.closing
            .lock()
            .pulls
            .insert((repo.to_string(), n), (p, Instant::now()));
    }

    /// Pull request `n` of `repo` as GitHub says, read as the person: a
    /// merged or closed one once, an open one again after [`PULL_AGAIN`].
    async fn pull(&self, service: &Arc<AgentService>, repo: &str, n: u64) -> Option<Pull> {
        let key = (repo.to_string(), n);
        if let Some((p, at)) = self.closing.lock().pulls.get(&key) {
            if !p.open || at.elapsed() < PULL_AGAIN {
                return Some(p.clone());
            }
        }
        let person = self.person(Some(service))?;
        let input = json!({ "repo": repo, "number": n });
        let read = workforce_agent::runner::CURRENT_USER_ID.scope(
            person,
            super::github::read(
                service.vault.clone(),
                service.global_vault.clone(),
                &service.config,
                &service.tools,
                super::github::PR,
                &input,
            ),
        );
        let got = match read.await {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(%repo, n, error = %workforce_desk::crew::one_line(&e, 200), "a finished session's pull request couldn't be read");
                return None;
            }
        };
        let p = Pull {
            open: got["state"] == "open",
            merged: got["merged"] == true,
            head: got["head_sha"].as_str().map(str::to_string),
            checks: got["checks"]["overall"].as_str().map(str::to_string),
            failed: got["checks"]["failure"].as_u64().unwrap_or(0),
            merged_at: got["merged_at"].as_str().map(str::to_string),
        };
        self.closing
            .lock()
            .pulls
            .insert(key, (p.clone(), Instant::now()));
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_closes_what_it_started_at_act_and_what_it_was_handed_only_at_act_fully() {
        use How::*;
        assert_eq!(what_to_do(Level::Full, Started), Some(Do::Close));
        assert_eq!(what_to_do(Level::Full, Handed), Some(Do::Close));
        assert_eq!(what_to_do(Level::Act, Started), Some(Do::Close));
        assert_eq!(what_to_do(Level::Act, Handed), Some(Do::Suggest));
        assert_eq!(what_to_do(Level::Suggest, Started), Some(Do::Suggest));
        assert_eq!(what_to_do(Level::Suggest, Handed), Some(Do::Suggest));
        // The person's own sessions, taken on by itself, never; and at
        // Watch it only looks.
        for level in [Level::Watch, Level::Suggest, Level::Act, Level::Full] {
            assert_eq!(what_to_do(level, Taken), None, "{level:?}");
        }
        assert_eq!(what_to_do(Level::Watch, Started), None);
        assert_eq!(what_to_do(Level::Watch, Handed), None);
    }

    fn pull(open: bool, merged: bool) -> Option<Pull> {
        Some(Pull {
            open,
            merged,
            head: Some("abc".into()),
            ..Default::default()
        })
    }

    #[test]
    fn done_is_its_pr_merged_its_tickets_done_its_word_or_a_day_with_nothing_open() {
        use TicketState::*;
        assert_eq!(
            finished(&[], &[(12, pull(false, true))], None, false),
            Some(Finished::Merged(12))
        );
        assert_eq!(
            finished(&[Done, Dropped], &[], None, false),
            Some(Finished::Ticket)
        );
        assert_eq!(
            finished(&[], &[], Some("the fix shipped"), false),
            Some(Finished::Said("the fix shipped".into()))
        );
        assert_eq!(finished(&[], &[], None, true), Some(Finished::Idle));
        assert_eq!(
            finished(&[], &[], None, false),
            None,
            "nothing says it's done"
        );
        // Anything open is work left, whatever else is done.
        assert_eq!(finished(&[Done, Review], &[], Some("done"), true), None);
        assert_eq!(
            finished(
                &[Done],
                &[(3, pull(false, true)), (4, pull(true, false))],
                None,
                true
            ),
            None
        );
        // A pull request closed unmerged isn't its work merged; one GitHub
        // couldn't say leaves the rest to decide.
        assert_eq!(finished(&[], &[(5, pull(false, false))], None, false), None);
        assert_eq!(
            finished(&[Done], &[(6, None)], None, false),
            Some(Finished::Ticket)
        );
        assert_eq!(finished(&[], &[(6, None)], None, false), None);
        assert_eq!(finished(&[], &[(6, None)], Some("done"), true), None);
        assert_eq!(
            finished(&[], &[(6, None), (7, pull(false, true))], None, true),
            Some(Finished::Merged(7))
        );
    }

    #[test]
    fn uncommitted_or_unpushed_work_keeps_it_open_unless_its_merged_head_is_head() {
        assert_eq!(held(&Folder::Nothing, &[]), None);
        let clean = |unpushed| Folder::Clean {
            head: Some("abc".into()),
            unpushed,
        };
        assert_eq!(held(&clean(0), &[]), None);
        assert_eq!(held(&clean(1), &[]), Some("1 commit not pushed".into()));
        assert_eq!(held(&clean(3), &[]), Some("3 commits not pushed".into()));
        // Its branch deleted on merge: what merged is its HEAD.
        assert_eq!(held(&clean(3), &["abc".into()]), None);
        assert_eq!(
            held(&clean(3), &["def".into()]),
            Some("3 commits not pushed".into())
        );
        assert_eq!(
            held(&Folder::Uncommitted(1), &["abc".into()]),
            Some("1 uncommitted file".into())
        );
        assert_eq!(
            held(&Folder::Uncommitted(4), &[]),
            Some("4 uncommitted files".into())
        );
        assert!(
            held(&Folder::Unread, &[]).is_some(),
            "what can't be read is kept"
        );
    }

    #[test]
    fn a_github_remote_and_an_accounts_pr_are_read_as_owner_name_and_number() {
        for url in [
            "https://github.com/driftwerk/workforce.git",
            "https://github.com/driftwerk/workforce",
            "git@github.com:driftwerk/workforce.git",
            "ssh://git@github.com/driftwerk/workforce",
        ] {
            assert_eq!(
                github_repo(url).as_deref(),
                Some("driftwerk/workforce"),
                "{url}"
            );
        }
        assert_eq!(github_repo("https://gitlab.com/a/b.git"), None);
        assert_eq!(github_repo("https://github.com/a"), None);
        assert_eq!(github_repo("https://github.com/../b"), None);
        let origin = Some("acme/web");
        assert_eq!(
            pull_ref("#624", None, origin),
            Some(("acme/web".into(), 624))
        );
        assert_eq!(
            pull_ref("624", None, origin),
            Some(("acme/web".into(), 624))
        );
        assert_eq!(
            pull_ref("driftwerk/workforce#434", None, origin),
            Some(("driftwerk/workforce".into(), 434))
        );
        assert_eq!(
            pull_ref(
                "#1",
                Some("https://github.com/driftwerk/workforce-desk/pull/307/files"),
                origin
            ),
            Some(("driftwerk/workforce-desk".into(), 307))
        );
        assert_eq!(
            pull_ref("#624", None, None),
            None,
            "no repository to read it in"
        );
        assert_eq!(pull_ref("the fix", None, origin), None);
    }

    async fn sh(dir: &Path, args: &[&str]) {
        let ok = tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn a_folder_is_read_for_what_closing_would_leave_behind() {
        let t = tempfile::tempdir().unwrap();
        let none = t.path().join("gone");
        assert_eq!(folder(none.to_str().unwrap()).await, Folder::Nothing);
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(folder(plain.to_str().unwrap()).await, Folder::Nothing);

        let remote = t.path().join("remote.git");
        let work = t.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            t.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .await;
        sh(&work, &["init", "-q"]).await;
        let w = work.to_str().unwrap();
        // Nothing committed yet, nothing changed: nothing to lose.
        assert_eq!(
            folder(w).await,
            Folder::Clean {
                head: None,
                unpushed: 0
            }
        );
        std::fs::write(work.join("a.txt"), "a").unwrap();
        std::fs::write(work.join(".DS_Store"), "x").unwrap();
        assert_eq!(
            folder(w).await,
            Folder::Uncommitted(1),
            "Finder's file isn't work"
        );
        sh(&work, &["add", "a.txt"]).await;
        sh(&work, &["commit", "-q", "-m", "a"]).await;
        std::fs::remove_file(work.join(".DS_Store")).unwrap();
        assert!(matches!(
            folder(w).await,
            Folder::Clean {
                unpushed: 1,
                head: Some(_)
            }
        ));
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        sh(&work, &["push", "-q", "origin", "HEAD:refs/heads/main"]).await;
        assert!(matches!(
            folder(w).await,
            Folder::Clean {
                unpushed: 0,
                head: Some(_)
            }
        ));
        // A planted fsmonitor never runs.
        let planted = t.path().join("ran");
        sh(
            &work,
            &[
                "config",
                "core.fsmonitor",
                &format!("touch {}", planted.display()),
            ],
        )
        .await;
        let _ = folder(w).await;
        assert!(
            !planted.exists(),
            "the repository's own config ran a command"
        );
    }

    #[test]
    fn its_why_reads_after_a_dash() {
        assert_eq!(Finished::Merged(12).why(), "Its PR #12 merged.");
        assert_eq!(
            Finished::Said("The fix shipped.".into()).why(),
            "It finished its work: The fix shipped."
        );
        assert_eq!(Finished::Said(" ".into()).why(), "It finished its work.");
        assert_eq!(
            workforce_desk::decision::clause(&Finished::Idle.why()).as_deref(),
            Some("it sat a day with nothing left to do")
        );
    }
}
