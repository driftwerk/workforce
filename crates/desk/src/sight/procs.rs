//! What a session's processes read as: the waits table. Pure.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use regex::Regex;

use super::{Kind, OpenCall, Reading, Wait};
use crate::model::Engine;
use crate::procs::{self, Proc, Stat};

/// The output lines that say a command waits, newest matched first: the
/// wait, the pattern (whole words, any case) and its words.
const SAID: &[(Wait, &str, &str)] = &[
    (
        Wait::Queue,
        r"all (\d+) (?P<noun>\w{1,20} )?slots? (are )?busy",
        "waiting for a ${noun}slot",
    ),
    (
        Wait::Lock,
        r"lock(ed)? by|waiting for (a |the )?(file )?lock|another .{1,40} is (running|in progress)",
        "waiting for a lock",
    ),
    (
        Wait::Queue,
        r"waiting (for|on) (?P<what>(a |an |the )?[\w -]{1,40})",
        "waiting for ${what}",
    ),
    (
        Wait::Queue,
        r"queued|in queue|position \d+ in (the )?queue",
        "queued",
    ),
    (Wait::Retry, r"retrying in \d+", "retrying"),
];

/// Programs that hold a lock while what they run runs.
const LOCKS: &[&str] = &["flock", "lockf", "lockrun", "with-lock"];

/// Programs that wait on the person at their terminal.
const ASKS: &[&str] = &[
    "sudo",
    "su",
    "ssh-askpass",
    "pinentry",
    "passwd",
    "vi",
    "vim",
    "nvim",
    "nano",
    "emacs",
    "less",
    "more",
];

/// Interpreters whose first argument is the script that is the program.
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "python", "python3", "node", "perl", "ruby",
];

/// Known long work, by program and the subcommands that make it so (none:
/// any).
const LONG: &[(&str, &[&str])] = &[
    ("cargo", &["build", "test", "check", "clippy", "nextest"]),
    ("npm", &["run", "test", "install", "build"]),
    ("pnpm", &["run", "test", "install", "build"]),
    ("yarn", &["run", "test", "install", "build"]),
    ("bun", &["run", "test", "install", "build"]),
    ("pytest", &[]),
    ("go", &["test", "build"]),
    ("make", &[]),
    ("docker", &["build"]),
    ("git", &["push", "fetch", "clone", "pull"]),
    ("curl", &[]),
    ("wget", &[]),
];

/// How much CPU a tree uses between two samples for it to be active.
const ACTIVE_SECS: u64 = 1;

/// How much earlier than its call a tool shell may read as started: `ps`
/// and a record both keep whole seconds.
pub const SKEW: Duration = Duration::seconds(2);

fn said() -> &'static [(Wait, Regex, &'static str)] {
    static SAID_RE: OnceLock<Vec<(Wait, Regex, &'static str)>> = OnceLock::new();
    SAID_RE.get_or_init(|| {
        SAID.iter()
            .map(|(wait, pattern, words)| {
                let re = Regex::new(&format!(r"(?i)\b(?:{pattern})\b")).expect("a valid pattern");
                (*wait, re, *words)
            })
            .collect()
    })
}

/// What an output line says its command waits on.
pub fn said_wait(line: &str) -> Option<(Wait, String)> {
    said().iter().find_map(|(wait, re, words)| {
        let caps = re.captures(line)?;
        let mut out = String::new();
        caps.expand(words, &mut out);
        Some((*wait, out.split_whitespace().collect::<Vec<_>>().join(" ")))
    })
}

/// What an open call's own output last said it waits on, and since when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// The call's id.
    pub call: String,
    pub wait: Wait,
    pub words: String,
    /// When the line that says it first appeared.
    pub since: DateTime<Utc>,
}

/// What `call`'s output says after `new` lines (oldest first) appeared in
/// it at `at`, `held` having been said before: the wait its newest new line
/// says, kept from when it was first said; with no new line, `held`, until
/// the call's own processes work again.
pub fn said_after(
    held: Option<Said>,
    call: &str,
    new: &[String],
    at: DateTime<Utc>,
    busy: bool,
) -> Option<Said> {
    let held = held.filter(|h| h.call == call);
    let Some(line) = new.last() else {
        return held.filter(|_| !busy);
    };
    let (wait, words) = said_wait(line)?;
    Some(match held {
        Some(h) if (h.wait, h.words.as_str()) == (wait, words.as_str()) => h,
        _ => Said {
            call: call.to_string(),
            wait,
            words,
            since: at,
        },
    })
}

/// The tool shell an open call runs in: the first of `tree`'s started at or
/// after the call.
pub fn call_shell<'a>(tree: &[&'a Stat], call: &OpenCall, now: DateTime<Utc>) -> Option<&'a Stat> {
    tree.iter()
        .copied()
        .filter(|s| procs::is_tool_shell(&s.proc))
        .filter(|s| started(now, s) >= call.since - SKEW)
        .max_by_key(|s| (s.elapsed_secs, std::cmp::Reverse(s.proc.pid)))
}

/// `head` and every process of `tree` under it.
pub fn subtree<'a>(tree: &[&'a Stat], head: &'a Stat) -> Vec<&'a Stat> {
    std::iter::once(head)
        .chain(procs::under(tree, head.proc.pid).into_iter().copied())
        .collect()
}

fn started(now: DateTime<Utc>, s: &Stat) -> DateTime<Utc> {
    now - Duration::seconds(i64::try_from(s.elapsed_secs).unwrap_or(i64::MAX / 2))
}

/// A process's program and the arguments after it: a script's own name
/// when an interpreter runs one.
fn command(p: &Proc) -> (String, Vec<&str>) {
    let mut words = p.args.split_whitespace();
    let program = p.program().unwrap_or_default();
    let _ = words.next();
    let rest: Vec<&str> = words.collect();
    match rest.first() {
        Some(script) if INTERPRETERS.contains(&program.as_str()) && !script.starts_with('-') => {
            let name = script.rsplit('/').next().unwrap_or(script).to_string();
            (name, rest[1..].to_vec())
        }
        _ => (program, rest),
    }
}

/// The processes that are a session's own work, root first: for Claude
/// Code, its tool shells and what runs under them (not its MCP servers);
/// for Codex, what its app-server started since `baseline`; for any other,
/// all under its root. Empty when the root isn't listed.
pub fn work<'a>(
    all: &'a [Stat],
    root: u32,
    engine: Engine,
    baseline: Option<&HashSet<u32>>,
) -> Vec<&'a Stat> {
    let Some(top) = all.iter().find(|s| s.proc.pid == root) else {
        return Vec::new();
    };
    let heads: Vec<u32> = match engine {
        Engine::Claude => procs::under(all, root)
            .into_iter()
            .filter(|s| procs::is_tool_shell(&s.proc))
            .map(|s| s.proc.pid)
            .collect(),
        Engine::Codex => procs::children(all, root)
            .filter(|s| baseline.is_none_or(|b| !b.contains(&s.proc.pid)))
            .map(|s| s.proc.pid)
            .collect(),
        Engine::Gemini
        | Engine::Copilot
        | Engine::OpenCode
        | Engine::Cursor
        | Engine::Devin
        | Engine::Custom => procs::children(all, root).map(|s| s.proc.pid).collect(),
    };
    let mut seen = HashSet::from([root]);
    let below = heads.into_iter().flat_map(|head| {
        all.iter()
            .find(|s| s.proc.pid == head)
            .into_iter()
            .chain(procs::under(all, head))
    });
    std::iter::once(top)
        .chain(
            below
                .filter(|s| seen.insert(s.proc.pid))
                .collect::<Vec<_>>(),
        )
        .collect()
}

/// Each process's run and CPU, as a sample keeps them.
pub fn runs(tree: &[&Stat]) -> HashMap<u32, (u64, u64)> {
    tree.iter()
        .map(|s| (s.proc.pid, (s.elapsed_secs, s.cpu_secs)))
        .collect()
}

/// How much CPU `tree` used since `prev`: a kept process's growth, and the
/// whole CPU of one new since (a smaller run is a number reused).
pub fn cpu_growth(prev: &HashMap<u32, (u64, u64)>, tree: &[&Stat]) -> u64 {
    tree.iter()
        .map(|s| match prev.get(&s.proc.pid) {
            Some((ran, cpu)) if *ran <= s.elapsed_secs => s.cpu_secs.saturating_sub(*cpu),
            _ => s.cpu_secs,
        })
        .sum()
}

/// Whether `tree` used the CPU since `prev`.
pub fn active(prev: &HashMap<u32, (u64, u64)>, tree: &[&Stat]) -> bool {
    cpu_growth(prev, tree) >= ACTIVE_SECS
}

/// Whether a session's work ([`work`], root first) used the CPU since
/// `prev`: what runs under its root, never the root, whose own screen keeps
/// it ticking.
pub fn work_active(prev: &HashMap<u32, (u64, u64)>, tree: &[&Stat]) -> bool {
    active(prev, tree.get(1..).unwrap_or_default())
}

/// What a classification reads from.
#[derive(Debug, Clone, Copy)]
pub struct Evidence<'a> {
    /// The session's work, root first ([`work`]).
    pub tree: &'a [&'a Stat],
    pub open_call: Option<&'a OpenCall>,
    /// What the open call's own output says it waits on.
    pub said: Option<&'a Said>,
    /// When its processes last used the CPU.
    pub active_at: DateTime<Utc>,
    pub now: DateTime<Utc>,
    /// How long a sleep loop runs idle before it is a wait.
    pub poll: Duration,
}

impl Evidence<'_> {
    fn started(&self, s: &Stat) -> DateTime<Utc> {
        started(self.now, s)
    }

    fn parent(&self, s: &Stat) -> Option<&Stat> {
        self.tree
            .iter()
            .copied()
            .find(|p| p.proc.pid == s.proc.ppid)
    }

    fn kids(&self, s: &Stat) -> impl Iterator<Item = &Stat> + '_ {
        let pid = s.proc.pid;
        self.tree
            .iter()
            .copied()
            .filter(move |p| p.proc.ppid == pid && p.proc.pid != pid)
    }

    fn depth(&self, s: &Stat) -> usize {
        std::iter::successors(Some(s), |p| self.parent(p))
            .take(64)
            .count()
    }

    fn reading(&self, kind: Kind, words: String, s: &Stat) -> Reading {
        Reading {
            kind,
            words,
            since: self.started(s),
            by: Some(command(&s.proc).0),
            on: None,
            pid: Some(s.proc.pid),
            call: None,
        }
    }
}

/// What a session's evidence reads as: the first rule of the waits table
/// that matches.
pub fn classify(e: &Evidence<'_>) -> Reading {
    let below: Vec<&Stat> = e.tree.iter().skip(1).copied().collect();
    said_rule(e, &below)
        .or_else(|| lock_rule(e, &below))
        .or_else(|| sleep_rule(e, &below))
        .or_else(|| input_rule(e, &below))
        .or_else(|| watch_rule(e, &below))
        .or_else(|| remote_rule(e, &below))
        .or_else(|| long_rule(e, &below))
        .unwrap_or_else(|| otherwise(e, &below))
}

fn said_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    let call = e.open_call?;
    let said = e.said.filter(|s| s.call == call.id)?;
    Some(Reading {
        kind: Kind::Waits(said.wait),
        words: said.words.clone(),
        since: said.since,
        by: Some(call.tool.clone()),
        on: None,
        pid: call_shell(below, call, e.now).map(|s| s.proc.pid),
        call: Some(call.id.clone()),
    })
}

fn lock_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    let lock = below
        .iter()
        .find(|s| LOCKS.contains(&command(&s.proc).0.as_str()))?;
    match e.kids(lock).next() {
        None => {
            let path = command(&lock.proc)
                .1
                .into_iter()
                .find(|a| !a.starts_with('-'))
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            Some(e.reading(
                Kind::Waits(Wait::Lock),
                format!("waiting for a lock{path}"),
                lock,
            ))
        }
        Some(child) => {
            let held: Vec<&Stat> = std::iter::once(*lock)
                .chain(std::iter::once(child))
                .chain(procs::under(e.tree, child.proc.pid).into_iter().copied())
                .collect();
            let inner = Evidence { tree: &held, ..*e };
            let read = classify(&inner);
            Some(Reading {
                words: format!("{}, under a lock", read.words),
                ..read
            })
        }
    }
}

fn sleep_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    if e.now - e.active_at < e.poll {
        return None;
    }
    let poll = u64::try_from(e.poll.num_seconds()).unwrap_or(0);
    below.iter().find_map(|s| {
        let leaf = e.kids(s).next().is_none() && command(&s.proc).0 == "sleep";
        let parent = e
            .parent(s)
            .filter(|p| p.elapsed_secs >= poll && !procs::is_tool_shell(&p.proc))?;
        leaf.then(|| Reading {
            since: e.active_at,
            ..e.reading(
                Kind::Waits(Wait::Retry),
                format!("waiting and retrying ({})", command(&parent.proc).0),
                parent,
            )
        })
    })
}

fn input_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    below.iter().find_map(|s| {
        let program = command(&s.proc).0;
        let asks = s.foreground
            && s.state == 'S'
            && e.kids(s).next().is_none()
            && ASKS.contains(&program.as_str());
        asks.then(|| {
            e.reading(
                Kind::Input,
                format!("may be waiting for input at its terminal ({program})"),
                s,
            )
        })
    })
}

/// What a watching command watches, in fixed words.
fn watches(program: &str, args: &[&str]) -> Option<&'static str> {
    let has = |w: &str| args.contains(&w);
    match program {
        "gh" if args.starts_with(&["run", "watch"]) => Some("a GitHub Actions run"),
        "gh" if args.starts_with(&["pr", "checks"]) && has("--watch") => Some("a PR's checks"),
        "kubectl" if args.starts_with(&["rollout", "status"]) => Some("a Kubernetes rollout"),
        "aws" if has("wait") => Some("an AWS change"),
        "docker" if args.starts_with(&["compose", "wait"]) => Some("containers"),
        _ => None,
    }
}

fn watch_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    below.iter().find_map(|s| {
        let (program, args) = command(&s.proc);
        watches(&program, &args).map(|what| e.reading(Kind::Watches, format!("watching {what}"), s))
    })
}

/// ssh's options that take a value.
const SSH_VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The host and command of an `ssh` (or `mosh`) command line.
fn remote(args: &[&str]) -> Option<(String, String)> {
    let mut words = args.iter();
    let host = loop {
        let w = words.next()?;
        match w.strip_prefix('-') {
            Some("-") => break words.next()?,
            Some(flags) => {
                let value_next = flags
                    .char_indices()
                    .find(|(_, f)| SSH_VALUED.contains(*f))
                    .is_some_and(|(at, f)| at + f.len_utf8() == flags.len());
                if value_next {
                    words.next()?;
                }
            }
            None => break w,
        }
    };
    let host = host.rsplit('@').next().unwrap_or(host).to_string();
    Some((host, words.copied().collect::<Vec<_>>().join(" ")))
}

fn remote_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    below.iter().find_map(|s| {
        let (program, args) = command(&s.proc);
        if !matches!(program.as_str(), "ssh" | "mosh") {
            return None;
        }
        let (host, run) = remote(&args)?;
        let words = match run.is_empty() {
            true => format!("connected to {host} over {program}"),
            false => format!(
                "running `{}` on {host} over {program}",
                crate::crew::clip(&run, 80)
            ),
        };
        Some(Reading {
            on: Some(host),
            ..e.reading(Kind::Remote, words, s)
        })
    })
}

fn long_rule(e: &Evidence<'_>, below: &[&Stat]) -> Option<Reading> {
    below
        .iter()
        .filter_map(|s| {
            let (program, args) = command(&s.proc);
            let (_, subs) = LONG.iter().find(|(p, _)| *p == program)?;
            let sub = args.iter().find(|a| !a.starts_with('-'));
            let what = match (subs.is_empty(), sub) {
                (true, _) => program,
                (false, Some(sub)) if subs.contains(sub) => format!("{program} {sub}"),
                (false, _) => return None,
            };
            Some((e.depth(s), s, what))
        })
        .max_by_key(|(depth, ..)| *depth)
        .map(|(_, s, what)| e.reading(Kind::Works, format!("running {what}"), s))
}

fn otherwise(e: &Evidence<'_>, below: &[&Stat]) -> Reading {
    let deepest = below.iter().max_by_key(|s| (e.depth(s), s.proc.pid));
    match (deepest, e.open_call) {
        (Some(s), _) => e.reading(Kind::Works, format!("running {}", command(&s.proc).0), s),
        (None, Some(call)) => Reading {
            kind: Kind::Works,
            words: format!("in its {} call", call.tool),
            since: call.since,
            by: Some(call.tool.clone()),
            on: None,
            pid: None,
            call: Some(call.id.clone()),
        },
        (None, None) => Reading::quiet(e.now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_000_000, 0).unwrap()
    }

    fn stat(pid: u32, ppid: u32, args: &str) -> Stat {
        Stat {
            proc: Proc {
                pid,
                ppid,
                args: args.into(),
            },
            state: 'S',
            foreground: false,
            elapsed_secs: 600,
            cpu_secs: 0,
            rss_kb: 1000,
        }
    }

    const SHELL: &str =
        "/bin/zsh -c source /h/.claude/shell-snapshots/snapshot-zsh-1.sh && eval 'x'";

    fn bash_call(ago: i64) -> OpenCall {
        OpenCall {
            id: "toolu_1".into(),
            tool: "Bash".into(),
            input: "~/bin/rbuild w -- cargo test".into(),
            since: now() - Duration::minutes(ago),
        }
    }

    fn appeared(call: &OpenCall, line: &str, ago: i64) -> Option<Said> {
        let at = now() - Duration::minutes(ago);
        said_after(None, &call.id, &[line.to_string()], at, false)
    }

    /// `all` read with an open Bash call whose output newly said `said`.
    fn read(all: &[Stat], said: Option<&str>, active_ago: i64) -> Reading {
        let tree = work(all, 100, Engine::Claude, None);
        let call = bash_call(11);
        let said = said.and_then(|line| appeared(&call, line, 10));
        classify(&Evidence {
            tree: &tree,
            open_call: said.as_ref().map(|_| &call),
            said: said.as_ref(),
            active_at: now() - Duration::minutes(active_ago),
            now: now(),
            poll: Duration::minutes(10),
        })
    }

    fn claude(rest: Vec<Stat>) -> Vec<Stat> {
        [
            stat(100, 1, "claude --session-id s1"),
            stat(150, 100, "node /mcp/server.js"),
            stat(101, 100, SHELL),
        ]
        .into_iter()
        .chain(rest)
        .collect()
    }

    const BUSY: &str = "rbuild: all 3 build slots are busy; waiting for one";

    #[test]
    fn a_full_build_queue_is_read_from_what_it_prints_however_it_is_reached() {
        let remote = claude(vec![stat(
            102,
            101,
            "ssh devbox ~/bin/rbuild-run w cargo test",
        )]);
        let r = read(&remote, Some(BUSY), 0);
        assert_eq!(r.kind, Kind::Waits(Wait::Queue));
        assert_eq!(r.words, "waiting for a build slot");
        assert_eq!(r.since, now() - Duration::minutes(10));
        let local = claude(vec![
            stat(102, 101, "/bin/bash /h/bin/rbuild-run w cargo test"),
            stat(103, 102, "sleep 10"),
        ]);
        assert_eq!(
            read(&local, Some(BUSY), 0).words,
            "waiting for a build slot"
        );
    }

    #[test]
    fn what_an_output_says_it_waits_on_is_read_in_its_own_words() {
        assert_eq!(
            said_wait("Waiting for the database to start"),
            Some((Wait::Queue, "waiting for the database to start".into()))
        );
        assert_eq!(
            said_wait("Blocking waiting for file lock on build directory"),
            Some((Wait::Lock, "waiting for a lock".into()))
        );
        assert_eq!(
            said_wait("job is queued"),
            Some((Wait::Queue, "queued".into()))
        );
        assert_eq!(
            said_wait("Retrying in 30 seconds"),
            Some((Wait::Retry, "retrying".into()))
        );
        assert_eq!(
            said_wait("all 2 slots busy"),
            Some((Wait::Queue, "waiting for a slot".into()))
        );
        assert_eq!(said_wait("everything is fine"), None);
    }

    #[test]
    fn only_the_newest_new_line_of_the_calls_output_says_a_wait_from_when_it_appeared() {
        let lines = |l: &[&str]| l.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let at = |min| now() + Duration::minutes(min);
        let said = said_after(None, "t1", &lines(&["Compiling x", BUSY]), at(0), true).unwrap();
        assert_eq!((said.wait, said.since), (Wait::Queue, at(0)));
        let kept = said_after(Some(said.clone()), "t1", &[], at(5), false);
        assert_eq!(kept.as_ref(), Some(&said), "nothing new keeps the wait");
        let again = said_after(Some(said.clone()), "t1", &lines(&[BUSY]), at(6), false);
        assert_eq!(
            again.map(|s| s.since),
            Some(at(0)),
            "said again, since first said"
        );
        assert_eq!(
            said_after(
                Some(said.clone()),
                "t1",
                &lines(&["test a ... ok"]),
                at(7),
                false
            ),
            None,
            "a newer line that says no wait ends it"
        );
        assert_eq!(
            said_after(
                Some(said.clone()),
                "t1",
                &lines(&[BUSY, "Compiling x"]),
                at(7),
                false
            ),
            None
        );
        assert_eq!(
            said_after(Some(said.clone()), "t1", &[], at(7), true),
            None,
            "its processes working again end it"
        );
        let other = said_after(Some(said), "t2", &lines(&[BUSY]), at(8), false).unwrap();
        assert_eq!((other.call.as_str(), other.since), ("t2", at(8)));
    }

    #[test]
    fn a_wait_reads_from_the_waiting_calls_own_shell_not_the_oldest_process() {
        let all = claude(vec![
            Stat {
                elapsed_secs: 3600,
                ..stat(102, 101, "npm run dev")
            },
            Stat {
                elapsed_secs: 60,
                ..stat(103, 100, SHELL)
            },
            Stat {
                elapsed_secs: 59,
                ..stat(104, 103, "ssh devbox ~/bin/rbuild-run w cargo test")
            },
        ]);
        let tree = work(&all, 100, Engine::Claude, None);
        let call = OpenCall {
            since: now() - Duration::seconds(61),
            ..bash_call(0)
        };
        let said = appeared(&call, BUSY, 0);
        let r = classify(&Evidence {
            tree: &tree,
            open_call: Some(&call),
            said: said.as_ref(),
            active_at: now(),
            now: now(),
            poll: Duration::minutes(10),
        });
        assert_eq!(r.kind, Kind::Waits(Wait::Queue));
        assert_eq!((r.pid, r.call.as_deref()), (Some(103), Some("toolu_1")));
        assert_eq!(r.since, now());
        assert_eq!(r.by.as_deref(), Some("Bash"));
        let later = OpenCall {
            since: now() - Duration::seconds(30),
            ..call
        };
        assert_eq!(
            call_shell(&tree, &later, now()),
            None,
            "no shell started since"
        );
    }

    #[test]
    fn a_secret_beside_a_wait_never_reaches_its_words() {
        let line = format!("token SECRET-abc: {BUSY}");
        let (_, words) = said_wait(&line).unwrap();
        assert_eq!(words, "waiting for a build slot");
        assert!(!words.contains("SECRET"));
    }

    fn idle() -> Vec<Stat> {
        vec![
            stat(100, 1, "claude --session-id s1"),
            stat(150, 100, "node /mcp/server.js"),
        ]
    }

    #[test]
    fn a_wait_said_with_no_open_call_or_by_another_call_is_no_wait() {
        let all = idle();
        let tree = work(&all, 100, Engine::Claude, None);
        let call = bash_call(11);
        let said = appeared(&call, BUSY, 10);
        let other = OpenCall {
            id: "toolu_2".into(),
            ..call.clone()
        };
        let evidence = |open_call| Evidence {
            tree: &tree,
            open_call,
            said: said.as_ref(),
            active_at: now(),
            now: now(),
            poll: Duration::minutes(10),
        };
        assert_eq!(classify(&evidence(None)).kind, Kind::Quiet);
        assert_eq!(classify(&evidence(Some(&other))).kind, Kind::Works);
        assert_eq!(
            classify(&evidence(Some(&call))).kind,
            Kind::Waits(Wait::Queue)
        );
    }

    #[test]
    fn a_lock_tool_waits_alone_and_reads_as_what_it_holds_with_a_child() {
        let waiting = claude(vec![stat(102, 101, "flock -x /tmp/build.lock make")]);
        let r = read(&waiting, None, 0);
        assert_eq!(r.kind, Kind::Waits(Wait::Lock));
        assert_eq!(r.words, "waiting for a lock (/tmp/build.lock)");
        let holding = claude(vec![
            stat(102, 101, "flock /tmp/build.lock cargo test"),
            stat(103, 102, "cargo test --workspace"),
        ]);
        let r = read(&holding, None, 0);
        assert_eq!(r.kind, Kind::Works);
        assert_eq!(r.words, "running cargo test, under a lock");
        assert_eq!(r.pid, Some(103));
    }

    #[test]
    fn a_sleep_loop_idle_past_the_poll_reads_as_waiting_and_retrying() {
        let polling = claude(vec![
            stat(102, 101, "/bin/bash ./wait-for-db.sh"),
            stat(103, 102, "sleep 10"),
        ]);
        let r = read(&polling, None, 11);
        assert_eq!(r.kind, Kind::Waits(Wait::Retry));
        assert_eq!(r.words, "waiting and retrying (wait-for-db.sh)");
        assert_eq!(r.since, now() - Duration::minutes(11));
        assert_eq!(read(&polling, None, 2).kind, Kind::Works);
        let young = claude(vec![
            Stat {
                elapsed_secs: 60,
                ..stat(102, 101, "/bin/bash ./wait-for-db.sh")
            },
            stat(103, 102, "sleep 10"),
        ]);
        assert_eq!(read(&young, None, 11).kind, Kind::Works);
        let bare = claude(vec![stat(102, 101, "sleep 600")]);
        assert_eq!(
            read(&bare, None, 11).kind,
            Kind::Works,
            "a sleep the tool runs itself is no loop"
        );
    }

    #[test]
    fn a_program_at_the_terminal_reads_as_waiting_for_input() {
        let sudo = claude(vec![Stat {
            foreground: true,
            ..stat(102, 101, "sudo apt install x")
        }]);
        let r = read(&sudo, None, 0);
        assert_eq!(r.kind, Kind::Input);
        assert_eq!(r.words, "may be waiting for input at its terminal (sudo)");
        let behind = claude(vec![stat(102, 101, "sudo apt install x")]);
        assert_ne!(read(&behind, None, 0).kind, Kind::Input);
        let editor = claude(vec![
            stat(102, 101, "git commit"),
            Stat {
                foreground: true,
                ..stat(103, 102, "vim .git/COMMIT_EDITMSG")
            },
        ]);
        assert_eq!(read(&editor, None, 0).kind, Kind::Input);
    }

    #[test]
    fn a_watcher_reads_as_watching() {
        for (args, what) in [
            ("gh run watch 123", "watching a GitHub Actions run"),
            ("gh pr checks 7 --watch", "watching a PR's checks"),
            (
                "kubectl rollout status deploy/x",
                "watching a Kubernetes rollout",
            ),
            ("aws ecs wait services-stable", "watching an AWS change"),
        ] {
            let r = read(&claude(vec![stat(102, 101, args)]), None, 0);
            assert_eq!((r.kind, r.words.as_str()), (Kind::Watches, what));
        }
    }

    #[test]
    fn ssh_alone_reads_as_remote_with_its_host_and_command() {
        let r = read(
            &claude(vec![stat(
                102,
                101,
                "ssh -p 22 dev@devbox ~/bin/rbuild-run w cargo test",
            )]),
            None,
            0,
        );
        assert_eq!(r.kind, Kind::Remote);
        assert_eq!(r.on.as_deref(), Some("devbox"));
        assert_eq!(
            r.words,
            "running `~/bin/rbuild-run w cargo test` on devbox over ssh"
        );
        let script = read(
            &claude(vec![stat(102, 101, "/bin/sh /t/bin/ssh devbox make")]),
            None,
            0,
        );
        assert_eq!(script.kind, Kind::Remote);
        let shell = read(&claude(vec![stat(102, 101, "ssh devbox")]), None, 0);
        assert_eq!(shell.words, "connected to devbox over ssh");
    }

    #[test]
    fn ssh_options_bundled_or_not_take_their_values_as_ssh_does() {
        let host = |line: &str| {
            let words: Vec<&str> = line.split_whitespace().collect();
            remote(&words).map(|(host, run)| format!("{host}|{run}"))
        };
        assert_eq!(host("-vp 22 dev@box make").as_deref(), Some("box|make"));
        assert_eq!(host("-p22 box").as_deref(), Some("box|"));
        assert_eq!(host("-vp22 box ls").as_deref(), Some("box|ls"));
        assert_eq!(host("-vo ProxyJump=x box").as_deref(), Some("box|"));
        assert_eq!(host("-oX=y box").as_deref(), Some("box|"));
        assert_eq!(host("-46v box").as_deref(), Some("box|"));
        assert_eq!(host("-i key -- box ls -l").as_deref(), Some("box|ls -l"));
        assert_eq!(host("-vp"), None);
    }

    #[test]
    fn known_long_work_reads_as_its_program_and_subcommand_deepest_first() {
        let tests = claude(vec![
            stat(102, 101, "make check"),
            stat(103, 102, "cargo test -p x"),
        ]);
        let r = read(&tests, None, 0);
        assert_eq!(
            (r.kind, r.words.as_str()),
            (Kind::Works, "running cargo test")
        );
        let other = claude(vec![stat(102, 101, "cargo fmt")]);
        assert_eq!(read(&other, None, 0).words, "running cargo");
    }

    #[test]
    fn with_nothing_known_it_reads_as_the_deepest_program_the_open_call_or_quiet() {
        let r = read(&claude(vec![stat(102, 101, "python3 -c 'x'")]), None, 0);
        assert_eq!(r.words, "running python3");
        assert_eq!(read(&idle(), None, 0).kind, Kind::Quiet);
        let call = OpenCall {
            id: "t1".into(),
            tool: "WebFetch".into(),
            input: "x".into(),
            since: now() - Duration::minutes(3),
        };
        let all = idle();
        let tree = work(&all, 100, Engine::Claude, None);
        let r = classify(&Evidence {
            tree: &tree,
            open_call: Some(&call),
            said: None,
            active_at: now(),
            now: now(),
            poll: Duration::minutes(10),
        });
        assert_eq!(
            (r.kind, r.words.as_str(), r.since),
            (Kind::Works, "in its WebFetch call", call.since)
        );
    }

    #[test]
    fn a_sessions_work_is_its_tool_shells_not_its_servers() {
        let all = claude(vec![stat(102, 101, "cargo test")]);
        let pids: Vec<u32> = work(&all, 100, Engine::Claude, None)
            .iter()
            .map(|s| s.proc.pid)
            .collect();
        assert_eq!(pids, [100, 101, 102]);
        assert!(work(&all, 999, Engine::Claude, None).is_empty());
        let codex = [
            stat(200, 1, "codex app-server"),
            stat(201, 200, "old worker"),
            stat(202, 200, "bash -c npm test"),
        ];
        let before = HashSet::from([201]);
        let pids: Vec<u32> = work(&codex, 200, Engine::Codex, Some(&before))
            .iter()
            .map(|s| s.proc.pid)
            .collect();
        assert_eq!(pids, [200, 202]);
    }

    #[test]
    fn cpu_is_counted_on_kept_and_new_processes_and_never_on_gone_ones() {
        let prev = HashMap::from([(10, (100, 5)), (11, (100, 50)), (12, (900, 0))]);
        let kept = Stat {
            elapsed_secs: 130,
            cpu_secs: 7,
            ..stat(10, 1, "a")
        };
        assert_eq!(cpu_growth(&prev, &[&kept]), 2);
        assert!(active(&prev, &[&kept]));
        let idle = Stat {
            cpu_secs: 5,
            ..kept.clone()
        };
        assert!(!active(&prev, &[&idle]), "less than a second is not active");
        assert!(!active(&prev, &[]), "a process that exited counts nothing");
        let reused = Stat {
            elapsed_secs: 3,
            cpu_secs: 2,
            ..stat(12, 1, "b")
        };
        assert_eq!(cpu_growth(&prev, &[&reused]), 2, "a reused number is new");
        let new = Stat {
            cpu_secs: 4,
            ..stat(13, 1, "c")
        };
        assert_eq!(cpu_growth(&prev, &[&new]), 4);
        assert_eq!(runs(&[&new]), HashMap::from([(13, (600, 4))]));
    }

    #[test]
    fn only_what_runs_under_the_root_is_work_not_the_ticking_root() {
        let ran = |cpu| Stat {
            cpu_secs: cpu,
            ..stat(100, 1, "claude --session-id s1")
        };
        let all = claude(vec![stat(102, 101, "cargo test -p x")]);
        let before = [vec![ran(40)], all[1..].to_vec()].concat();
        let after = [vec![ran(45)], all[1..].to_vec()].concat();
        let prev = runs(&work(&before, 100, Engine::Claude, None));
        let tree = work(&after, 100, Engine::Claude, None);
        assert!(active(&prev, &tree), "the root's own CPU grew");
        assert!(!work_active(&prev, &tree), "nothing under it did");
        let busy: Vec<Stat> = after
            .iter()
            .map(|s| match s.proc.pid {
                102 => Stat {
                    cpu_secs: 9,
                    ..s.clone()
                },
                _ => s.clone(),
            })
            .collect();
        assert!(work_active(&prev, &work(&busy, 100, Engine::Claude, None)));
        assert!(!work_active(&prev, &[]));
    }

    #[test]
    fn the_output_rule_comes_before_every_other() {
        let all = claude(vec![
            stat(102, 101, "cargo test"),
            Stat {
                foreground: true,
                ..stat(103, 101, "sudo x")
            },
        ]);
        assert_eq!(read(&all, Some(BUSY), 0).kind, Kind::Waits(Wait::Queue));
        assert_eq!(read(&all, None, 0).kind, Kind::Input);
    }
}
