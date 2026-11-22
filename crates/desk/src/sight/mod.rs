//! What Desk sees inside a session from evidence, without asking it: what
//! runs under it and what that waits on, read by Desk's own background
//! pass ([`crate::service::Desk::sight_pass`]), and whether that is a
//! stall ([`verdict`]). Pure; the reading is sampled by the service.

pub mod logs;
pub mod procs;
pub mod transcript;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::record::Part;
use crate::model::SessionState;

/// What a wait waits on, in fixed words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    Queue,
    Lock,
    Retry,
}

/// What a session's evidence reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "wait", rename_all = "snake_case")]
pub enum Kind {
    Waits(Wait),
    Works,
    Watches,
    Remote,
    Input,
    Quiet,
}

impl Kind {
    /// Whether it holds the session up on something other than its own
    /// work: a wait, its terminal, or a command on another computer.
    pub fn holds_up(self) -> bool {
        matches!(self, Self::Waits(_) | Self::Input | Self::Remote)
    }
}

/// What the evidence says a session is doing now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    #[serde(flatten)]
    pub kind: Kind,
    /// In words taken from what the session runs or prints.
    pub words: String,
    pub since: DateTime<Utc>,
    /// The process or call it was read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The computer a remote command runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// The process it was read from.
    #[serde(skip)]
    pub pid: Option<u32>,
    /// The open call it was read from: the waiting call's id.
    #[serde(skip)]
    pub call: Option<String>,
}

impl Reading {
    /// Nothing seen running at `now`.
    pub fn quiet(now: DateTime<Utc>) -> Self {
        Self {
            kind: Kind::Quiet,
            words: String::new(),
            since: now,
            by: None,
            on: None,
            pid: None,
            call: None,
        }
    }
}

/// A tool call a session's record shows started with nothing back yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCall {
    pub id: String,
    pub tool: String,
    pub input: String,
    pub since: DateTime<Utc>,
}

/// The newest call still open at the end of `lines`, each a record line's
/// time and parts, oldest first: a call is closed by its result, and every
/// call by the turn's end or the next prompt.
pub fn open_call(lines: impl IntoIterator<Item = (DateTime<Utc>, Vec<Part>)>) -> Option<OpenCall> {
    lines
        .into_iter()
        .fold(Vec::<OpenCall>::new(), |mut open, (at, parts)| {
            parts.into_iter().for_each(|part| match part {
                Part::Call {
                    id, tool, input, ..
                } => open.push(OpenCall {
                    id,
                    tool,
                    input,
                    since: at,
                }),
                Part::Result { id, .. } => open.retain(|c| c.id != id),
                Part::Ask { .. } | Part::End { .. } | Part::Compacted { .. } => open.clear(),
                Part::Text(_) => {}
            });
            open
        })
        .pop()
}

/// `d` in a few words: "45 s", "12 min", "2 h 50 min", "3 d 4 h".
pub fn lasted(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    let (days, hours, mins) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// The most a glance's rendering holds.
pub const GLANCE_CHARS: usize = 1_500;
/// The longest open call's input a glance says.
const INPUT_SAID: usize = 120;
/// The longest output line a glance says.
const OUTPUT_SAID: usize = 200;

/// When a session's records last grew, and how many it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Growth {
    pub grew_at: DateTime<Utc>,
    pub records: usize,
}

/// A pull request's CI, as a glance says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullGlance {
    pub number: u64,
    /// `success`, `failure`, `pending` or `none`.
    pub checks: String,
    pub failed: Vec<String>,
    /// Whether its head is the folder's own HEAD; none when either isn't
    /// known.
    pub head_matches_local: Option<bool>,
}

/// A session's folder, as a glance says it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceGlance {
    pub branch: Option<String>,
    pub unpushed: Option<u32>,
    pub uncommitted: usize,
    pub behind: Option<u32>,
    /// What closing it would leave behind, in closing's words.
    pub left: Option<String>,
    pub pr: Option<PullGlance>,
    /// What of it couldn't be read, in words.
    pub note: Option<String>,
}

/// What kind of stall the evidence shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallKind {
    Queued,
    Input,
    Still,
    CiRed,
    Left,
}

impl StallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Input => "input",
            Self::Still => "still",
            Self::CiRed => "ci_red",
            Self::Left => "left",
        }
    }
}

/// A stall the evidence shows: since when, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stall {
    pub kind: StallKind,
    pub since: DateTime<Utc>,
    pub words: String,
}

/// How long a session may go without moving (`quiet`) and wait (`stall`)
/// before it reads as stalled; zero turns that row off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub quiet: Duration,
    pub stall: Duration,
}

/// What Desk sees inside one session now, from its last sample and its
/// folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Glance {
    pub reading: Reading,
    pub open_call: Option<OpenCall>,
    pub transcript: Option<Growth>,
    /// When anything under it last used the CPU; none when its processes
    /// couldn't be listed.
    pub cpu_active_at: Option<DateTime<Utc>>,
    /// When its output last changed, and its newest line.
    pub output_at: Option<DateTime<Utc>>,
    pub output: Option<String>,
    /// How many background tasks it waits on while its turn is done.
    pub background: u32,
    /// Whether its samples span long enough to say it is still.
    pub settled: bool,
    /// When its state last changed.
    pub since: DateTime<Utc>,
    /// When it was last sampled; none before its first sample.
    pub as_of: Option<DateTime<Utc>>,
    /// Whether that sample is older than a read would take as fresh.
    pub aged: bool,
    pub workspace: Option<WorkspaceGlance>,
    pub stall: Option<Stall>,
}

/// The stall `g` shows of a session in `state` at `now`, if any. Evidence
/// not yet sampled is never a stall.
pub fn verdict(
    g: &Glance,
    state: SessionState,
    now: DateTime<Utc>,
    t: Thresholds,
) -> Option<Stall> {
    let past = |since: DateTime<Utc>, limit: Duration| !limit.is_zero() && now - since >= limit;
    let stall = |kind, since, words: String| Some(Stall { kind, since, words });
    let r = &g.reading;
    let sampled = g.as_of.is_some();
    let busy = state == SessionState::Working || (state == SessionState::Done && g.background > 0);
    let waited = match r.kind {
        Kind::Waits(_) if sampled && past(r.since, t.stall) => {
            let d = lasted(now - r.since);
            let words = match r.words.strip_prefix("waiting for ") {
                Some(what) => format!("has waited {d} for {what}"),
                None if r.words.is_empty() => format!("has waited {d}"),
                None => format!("has been {} for {d}", r.words),
            };
            stall(StallKind::Queued, r.since, words)
        }
        Kind::Input if sampled && past(r.since, t.quiet) => stall(
            StallKind::Input,
            r.since,
            format!(
                "{} for {}",
                Some(r.words.as_str())
                    .filter(|w| !w.is_empty())
                    .unwrap_or("may be waiting for input at its terminal"),
                lasted(now - r.since)
            ),
        ),
        _ => None,
    };
    let still = || {
        let cpu = g.cpu_active_at.filter(|_| busy && g.settled)?;
        let moved = [g.transcript.as_ref().map(|t| t.grew_at), g.output_at]
            .into_iter()
            .flatten()
            .fold(cpu, std::cmp::max);
        let quiet = matches!(r.kind, Kind::Works | Kind::Remote | Kind::Quiet);
        (quiet && past(moved, t.quiet)).then(|| Stall {
            kind: StallKind::Still,
            since: moved,
            words: format!(
                "nothing has moved for {}: no transcript growth, no output, no CPU under it{}",
                lasted(now - moved),
                match (r.kind, r.on.as_deref()) {
                    (Kind::Remote, Some(host)) =>
                        format!("; its ssh command may still run on {host}"),
                    _ => String::new(),
                }
            ),
        })
    };
    let workspace = || {
        let w = g.workspace.as_ref()?;
        let red = w.pr.as_ref().filter(|p| {
            state != SessionState::Working
                && p.checks == "failure"
                && p.head_matches_local == Some(true)
        });
        match red {
            Some(p) => stall(
                StallKind::CiRed,
                g.since,
                format!("its PR #{}'s CI failed ({})", p.number, named(&p.failed)),
            ),
            None => w
                .left
                .clone()
                .filter(|_| {
                    matches!(
                        state,
                        SessionState::Done
                            | SessionState::Idle
                            | SessionState::Stopped
                            | SessionState::Closed
                    )
                })
                .and_then(|left| stall(StallKind::Left, g.since, left)),
        }
    };
    waited.or_else(still).or_else(workspace)
}

/// Failed checks' names, or that none were named.
fn named(failed: &[String]) -> String {
    match failed {
        [] => "no check named".into(),
        names => names.join(", "),
    }
}

impl Glance {
    /// What it is doing now, in one line, times said in `tz`.
    pub fn line(&self, tz: chrono_tz::Tz, now: DateTime<Utc>) -> String {
        let hm = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
        let r = &self.reading;
        let output = self
            .output
            .as_deref()
            .filter(|_| matches!(r.kind, Kind::Waits(_)))
            .map(|o| format!(" (its output: \"{}\")", crate::crew::clip(o, OUTPUT_SAID)))
            .unwrap_or_default();
        let doing = match (&self.open_call, r.kind) {
            _ if self.as_of.is_none() => "Desk hasn't sampled it yet".to_string(),
            (Some(c), kind) => {
                let input = Some(crate::crew::clip(&c.input, INPUT_SAID))
                    .filter(|i| !i.is_empty())
                    .map(|i| format!(" `{i}`"))
                    .unwrap_or_default();
                let what = match kind {
                    Kind::Quiet => String::new(),
                    _ => format!(": {}{output}", r.words),
                };
                format!(
                    "its {} call{input} has run {}{what}",
                    c.tool,
                    lasted(now - c.since)
                )
            }
            (None, Kind::Quiet) => "nothing runs under it".to_string(),
            (None, _) => format!("{} for {}{output}", r.words, lasted(now - r.since)),
        };
        let grew = self
            .transcript
            .as_ref()
            .map(|t| format!("its transcript last grew at {}", hm(t.grew_at)));
        let cpu = self
            .cpu_active_at
            .map(|at| match now - at < Duration::minutes(2) {
                true => "it is using CPU".to_string(),
                false => format!("nothing under it used CPU since {}", hm(at)),
            });
        let aged = self
            .as_of
            .filter(|_| self.aged)
            .map(|at| format!("as of {}", hm(at)));
        let parts: Vec<String> = [Some(doing), grew, cpu, aged]
            .into_iter()
            .flatten()
            .collect();
        format!("{}.", parts.join("; "))
    }

    /// All it says, within [`GLANCE_CHARS`], times said in `tz`.
    pub fn render(&self, tz: chrono_tz::Tz, now: DateTime<Utc>) -> String {
        let hm = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
        let r = &self.reading;
        let by = [
            r.by.as_deref().map(|b| format!("by {b}")),
            r.on.as_deref().map(|h| format!("on {h}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let reading = (r.kind != Kind::Quiet).then(|| {
            format!(
                "Reads as: {} since {} ({}){}",
                r.words,
                hm(r.since),
                lasted(now - r.since),
                Some(by)
                    .filter(|b| !b.is_empty())
                    .map(|b| format!(", {b}"))
                    .unwrap_or_default()
            )
        });
        let output = self.output.as_deref().map(|o| {
            format!(
                "Newest output{}: \"{}\"",
                self.output_at
                    .map(|at| format!(" (changed {})", hm(at)))
                    .unwrap_or_default(),
                crate::crew::clip(o, OUTPUT_SAID)
            )
        });
        let records = self.transcript.as_ref().map(|t| {
            format!(
                "Transcript: {} record{}, last grew {} ({} ago)",
                t.records,
                if t.records == 1 { "" } else { "s" },
                hm(t.grew_at),
                lasted(now - t.grew_at)
            )
        });
        let background = (self.background > 0)
            .then(|| format!("Waits on {} background task(s) of its own", self.background));
        let workspace = self.workspace.as_ref().map(|w| {
            let counts = [
                w.branch.as_deref().map(|b| format!("branch {b}")),
                w.unpushed.map(|n| format!("{n} not pushed")),
                Some(format!("{} uncommitted", w.uncommitted)),
                w.behind.filter(|n| *n > 0).map(|n| format!("{n} behind")),
                w.pr.as_ref().map(|p| {
                    format!(
                        "PR #{}: CI {}{}{}",
                        p.number,
                        p.checks,
                        Some(&p.failed)
                            .filter(|f| !f.is_empty())
                            .map(|f| format!(" ({})", f.join(", ")))
                            .unwrap_or_default(),
                        match p.head_matches_local {
                            Some(false) => ", for another push than its HEAD",
                            _ => "",
                        }
                    )
                }),
                w.note.clone(),
            ];
            format!(
                "Workspace: {}",
                counts.into_iter().flatten().collect::<Vec<_>>().join(", ")
            )
        });
        let stall = self.stall.as_ref().map(|s| format!("Stall: {}", s.words));
        let lines: Vec<String> = [
            Some(format!("Now: {}", self.line(tz, now))),
            reading,
            output,
            records,
            background,
            workspace,
            stall,
        ]
        .into_iter()
        .flatten()
        .collect();
        crate::crew::clip(&lines.join("\n"), GLANCE_CHARS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::Asker;

    fn at(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_000_000, 0).unwrap() + Duration::minutes(min)
    }

    fn call(id: &str) -> Part {
        Part::Call {
            id: id.into(),
            tool: "Bash".into(),
            input: format!("run {id}"),
            files: vec![],
            command: None,
        }
    }

    fn result(id: &str) -> Part {
        Part::Result {
            id: id.into(),
            text: "ok".into(),
            failed: false,
        }
    }

    #[test]
    fn the_newest_call_with_no_result_is_the_open_one() {
        let open = open_call([
            (at(0), vec![call("a"), call("b")]),
            (at(1), vec![result("b")]),
        ])
        .unwrap();
        assert_eq!((open.id.as_str(), open.since), ("a", at(0)));
        assert_eq!(open.input, "run a");
        assert_eq!(
            open_call([(at(0), vec![call("a")]), (at(1), vec![result("a")])]),
            None
        );
    }

    #[test]
    fn a_turns_end_or_a_new_prompt_closes_every_call() {
        assert_eq!(
            open_call([
                (at(0), vec![call("a")]),
                (at(1), vec![Part::End { at: at(1) }])
            ]),
            None
        );
        let ask = Part::Ask {
            key: "k".into(),
            at: at(2),
            asker: Asker::Person,
            text: "next".into(),
        };
        assert_eq!(
            open_call([(at(0), vec![call("a")]), (at(2), vec![ask])]),
            None
        );
        assert_eq!(open_call(Vec::new()), None);
    }

    #[test]
    fn a_duration_is_said_in_its_largest_units() {
        assert_eq!(lasted(Duration::seconds(45)), "45 s");
        assert_eq!(lasted(Duration::minutes(12)), "12 min");
        assert_eq!(lasted(Duration::minutes(170)), "2 h 50 min");
        assert_eq!(lasted(Duration::hours(3)), "3 h");
        assert_eq!(lasted(Duration::hours(76)), "3 d 4 h");
        assert_eq!(lasted(Duration::days(2)), "2 d");
        assert_eq!(lasted(Duration::seconds(-5)), "0 s");
    }

    #[test]
    fn a_reading_is_written_with_its_kind_and_wait_beside_its_words() {
        let r = Reading {
            kind: Kind::Waits(Wait::Queue),
            words: "waiting for a build slot".into(),
            since: at(0),
            by: Some("ssh".into()),
            on: None,
            pid: Some(7),
            call: Some("toolu_1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "waits");
        assert_eq!(v["wait"], "queue");
        assert!(v.get("pid").is_none() && v.get("call").is_none());
        let back: Reading = serde_json::from_value(v).unwrap();
        assert_eq!(
            back,
            Reading {
                pid: None,
                call: None,
                ..r
            }
        );
        let works = serde_json::to_value(Reading::quiet(at(0))).unwrap();
        assert_eq!(works["kind"], "quiet");
        assert!(works.get("wait").is_none());
    }

    const T: Thresholds = Thresholds {
        quiet: Duration::minutes(30),
        stall: Duration::minutes(60),
    };

    fn reading(kind: Kind, words: &str, since: i64) -> Reading {
        Reading {
            kind,
            words: words.into(),
            since: at(since),
            by: None,
            on: None,
            pid: None,
            call: None,
        }
    }

    /// A glance sampled for 20 min by `at(0)`, everything last moving at
    /// `moved`.
    fn glance(r: Reading, moved: i64) -> Glance {
        Glance {
            reading: r,
            open_call: None,
            transcript: Some(Growth {
                grew_at: at(moved),
                records: 1,
            }),
            cpu_active_at: Some(at(moved)),
            output_at: Some(at(moved)),
            output: None,
            background: 0,
            settled: true,
            since: at(-200),
            as_of: Some(at(0)),
            aged: false,
            workspace: None,
            stall: None,
        }
    }

    fn kind(g: &Glance, state: SessionState, now: i64, t: Thresholds) -> Option<StallKind> {
        verdict(g, state, at(now), t).map(|s| s.kind)
    }

    const BUILD_SLOT: &str = "waiting for a build slot";

    #[test]
    fn a_wait_is_a_stall_from_exactly_stall_minutes_in_any_state_with_a_reading() {
        let g = glance(reading(Kind::Waits(Wait::Queue), BUILD_SLOT, 0), 0);
        assert_eq!(kind(&g, SessionState::Working, 59, T), None);
        let s = verdict(&g, SessionState::Working, at(60), T).unwrap();
        assert_eq!(s.kind, StallKind::Queued);
        assert_eq!(s.since, at(0));
        assert_eq!(s.words, "has waited 1 h for a build slot");
        let bg = Glance {
            background: 1,
            ..g.clone()
        };
        assert_eq!(
            kind(&bg, SessionState::Done, 170, T),
            Some(StallKind::Queued)
        );
        let retry = glance(
            reading(
                Kind::Waits(Wait::Retry),
                "waiting and retrying (poll.sh)",
                0,
            ),
            0,
        );
        assert_eq!(
            verdict(&retry, SessionState::Working, at(60), T)
                .unwrap()
                .words,
            "has been waiting and retrying (poll.sh) for 1 h"
        );
        let off = Thresholds {
            stall: Duration::zero(),
            ..T
        };
        assert_eq!(kind(&g, SessionState::Working, 600, off), None);
    }

    #[test]
    fn waiting_at_its_terminal_is_a_stall_after_quiet_minutes() {
        let words = "may be waiting for input at its terminal (sudo)";
        let g = glance(reading(Kind::Input, words, 0), 0);
        assert_eq!(kind(&g, SessionState::Working, 29, T), None);
        assert_eq!(
            verdict(&g, SessionState::Working, at(30), T).unwrap().words,
            format!("{words} for 30 min")
        );
    }

    #[test]
    fn a_working_session_where_nothing_moved_is_still_once_its_samples_have_settled() {
        let g = glance(reading(Kind::Quiet, "", 0), 0);
        assert_eq!(kind(&g, SessionState::Working, 29, T), None);
        let s = verdict(&g, SessionState::Working, at(30), T).unwrap();
        assert_eq!(s.kind, StallKind::Still);
        assert_eq!(
            s.words,
            "nothing has moved for 30 min: no transcript growth, no output, no CPU under it"
        );
        let single = Glance {
            settled: false,
            ..g.clone()
        };
        assert_eq!(
            kind(&single, SessionState::Working, 90, T),
            None,
            "one sample is never still"
        );
        let unlisted = Glance {
            cpu_active_at: None,
            ..g.clone()
        };
        assert_eq!(kind(&unlisted, SessionState::Working, 90, T), None);
        let grew = Glance {
            transcript: Some(Growth {
                grew_at: at(80),
                records: 1,
            }),
            ..g.clone()
        };
        assert_eq!(
            kind(&grew, SessionState::Working, 90, T),
            None,
            "a growing transcript moves"
        );
        assert_eq!(kind(&g, SessionState::Idle, 90, T), None);
        let off = Thresholds {
            quiet: Duration::zero(),
            ..T
        };
        assert_eq!(kind(&g, SessionState::Working, 900, off), None);
        let remote = Reading {
            on: Some("devbox".into()),
            ..reading(Kind::Remote, "connected to devbox over ssh", 0)
        };
        assert!(
            verdict(&glance(remote, 0), SessionState::Working, at(30), T)
                .unwrap()
                .words
                .ends_with("; its ssh command may still run on devbox")
        );
    }

    #[test]
    fn work_using_the_cpu_and_watching_are_never_stalls_however_long() {
        let works = glance(reading(Kind::Works, "running cargo", 0), 600);
        assert_eq!(kind(&works, SessionState::Working, 601, T), None);
        let watches = glance(reading(Kind::Watches, "watching a CI run", 0), 0);
        assert_eq!(kind(&watches, SessionState::Working, 900, T), None);
    }

    #[test]
    fn evidence_not_yet_sampled_is_never_a_stall() {
        let g = Glance {
            as_of: None,
            ..glance(reading(Kind::Waits(Wait::Queue), BUILD_SLOT, 0), 0)
        };
        assert_eq!(kind(&g, SessionState::Working, 900, T), None);
    }

    fn with_workspace(w: WorkspaceGlance) -> Glance {
        Glance {
            workspace: Some(w),
            ..glance(reading(Kind::Quiet, "", 0), 0)
        }
    }

    fn red(matches: Option<bool>) -> WorkspaceGlance {
        WorkspaceGlance {
            pr: Some(PullGlance {
                number: 12,
                checks: "failure".into(),
                failed: vec!["test".into(), "clippy".into()],
                head_matches_local: matches,
            }),
            ..WorkspaceGlance::default()
        }
    }

    #[test]
    fn red_ci_on_its_own_head_is_a_stall_once_it_stopped_working() {
        let g = with_workspace(red(Some(true)));
        let s = verdict(&g, SessionState::Done, at(1), T).unwrap();
        assert_eq!((s.kind, s.since), (StallKind::CiRed, at(-200)));
        assert_eq!(s.words, "its PR #12's CI failed (test, clippy)");
        assert_eq!(kind(&g, SessionState::Working, 1, T), None);
        let older = with_workspace(red(Some(false)));
        assert_eq!(
            kind(&older, SessionState::Done, 1, T),
            None,
            "CI for an older push"
        );
    }

    #[test]
    fn work_left_behind_is_said_in_closings_words_once_it_ended() {
        let g = with_workspace(WorkspaceGlance {
            left: Some("2 commits not pushed".into()),
            ..WorkspaceGlance::default()
        });
        for state in [
            SessionState::Done,
            SessionState::Idle,
            SessionState::Stopped,
            SessionState::Closed,
        ] {
            let s = verdict(&g, state, at(1), T).unwrap();
            assert_eq!(
                (s.kind, s.words.as_str()),
                (StallKind::Left, "2 commits not pushed")
            );
        }
        assert_eq!(kind(&g, SessionState::Working, 1, T), None);
    }

    fn tz() -> chrono_tz::Tz {
        chrono_tz::UTC
    }

    #[test]
    fn a_glances_line_names_the_open_call_its_wait_its_output_and_when_things_last_moved() {
        let g = Glance {
            open_call: Some(OpenCall {
                id: "t1".into(),
                tool: "Bash".into(),
                input: "~/bin/rbuild w -- cargo test".into(),
                since: at(-170),
            }),
            output: Some("rbuild: all 3 build slots are busy; waiting for one".into()),
            ..glance(reading(Kind::Waits(Wait::Queue), BUILD_SLOT, -170), -169)
        };
        let hm = |m: i64| at(m).format("%H:%M").to_string();
        assert_eq!(
            g.line(tz(), at(0)),
            format!(
                "its Bash call `~/bin/rbuild w -- cargo test` has run 2 h 50 min: waiting for a build slot \
                 (its output: \"rbuild: all 3 build slots are busy; waiting for one\"); its transcript last \
                 grew at {}; nothing under it used CPU since {}.",
                hm(-169),
                hm(-169)
            )
        );
        let unsampled = Glance {
            as_of: None,
            transcript: None,
            cpu_active_at: None,
            ..g.clone()
        };
        assert_eq!(unsampled.line(tz(), at(0)), "Desk hasn't sampled it yet.");
        let aged = Glance { aged: true, ..g };
        assert!(aged
            .line(tz(), at(0))
            .ends_with(&format!("; as of {}.", hm(0))));
    }

    #[test]
    fn a_glance_renders_within_its_budget_with_its_workspace_and_stall() {
        let mut g = with_workspace(WorkspaceGlance {
            branch: Some("dw-1".into()),
            unpushed: Some(2),
            uncommitted: 1,
            behind: Some(3),
            ..red(Some(true))
        });
        g.stall = verdict(&g, SessionState::Done, at(1), T);
        let text = g.render(tz(), at(1));
        assert!(text.contains("Workspace: branch dw-1, 2 not pushed, 1 uncommitted, 3 behind, PR #12: CI failure (test, clippy)"), "{text}");
        assert!(text.contains("Stall: its PR #12's CI failed"), "{text}");
        g.output = Some("x".repeat(5_000));
        g.reading = reading(Kind::Works, &"y".repeat(5_000), 0);
        assert!(g.render(tz(), at(1)).chars().count() <= GLANCE_CHARS);
    }
}
