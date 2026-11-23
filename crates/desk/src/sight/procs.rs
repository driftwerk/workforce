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

/// The tool whose calls run in a shell of their own.
pub const SHELL_TOOL: &str = "Bash";

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

/// What an open call's or a background shell's own output last said it
/// waits on, and since when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// The call's or the shell's id.
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

/// The tool shell each output is written from, by its id, no process given
/// to two, as `tree` was listed from `listed` on. Each background shell
/// (`shells`, by id and when its output file was born) is placed in one
/// started within a second after its file's birth and at most [`SKEW`]
/// before it; so is the open call, when it is a Bash call, by its own
/// output file's birth (`call_born`), and with none yet by when it began, in
/// one started at most [`SKEW`] before that, however long after (it may
/// wait on the person's approval first). A call of any other tool runs in
/// no shell of its own. All are ranked together in the whole seconds `ps`
/// keeps of a run: the nearest first, one started before ahead of one
/// after. Those as near in the same second are paired in order, the latest
/// of them with the latest started, so a shell started that second before
/// them all (a sub-agent's) is the one left; and with fewer shells than
/// them, one whose output's last line says it ended (`ended`) is the one
/// without. One with none placed waits in no process Desk knows.
pub fn placed<'a>(
    tree: &[&'a Stat],
    call: Option<&OpenCall>,
    call_born: Option<DateTime<Utc>>,
    shells: &[(&str, DateTime<Utc>)],
    ended: &[&str],
    listed: DateTime<Utc>,
) -> HashMap<String, &'a Stat> {
    let tools: Vec<&Stat> = tree
        .iter()
        .copied()
        .filter(|s| procs::is_tool_shell(&s.proc))
        .collect();
    let tools = &tools;
    // A shell whose start can't be said is placed for none.
    let ran = |s: &Stat| {
        started(listed, s)?;
        i64::try_from(s.elapsed_secs).ok()
    };
    let age = |at: DateTime<Utc>| (listed - at).num_milliseconds().div_euclid(1000);
    // Each one by when it began, and the earliest its shell may start
    // after it, in seconds before it.
    let mut items: Vec<(&str, DateTime<Utc>, i64)> =
        shells.iter().map(|&(id, made)| (id, made, -1)).collect();
    items.extend(
        call.filter(|c| c.tool == SHELL_TOOL)
            .map(|c| match call_born {
                Some(born) => (c.id.as_str(), born, -1),
                None => (c.id.as_str(), c.since, i64::MIN),
            }),
    );
    items.sort_by_key(|&(id, at, _)| (at, id));
    let fits = |low: i64, before: i64| (low..=SKEW.num_seconds()).contains(&before);
    let mut levels: Vec<(i64, i64)> = items
        .iter()
        .flat_map(|&(_, at, low)| {
            let a = age(at);
            tools.iter().filter_map(move |s| {
                let before = ran(s)?.checked_sub(a)?;
                fits(low, before).then_some((before, a))
            })
        })
        .collect();
    levels.sort_by_key(|&(before, a)| (before.abs(), before < 0, a));
    levels.dedup();
    levels
        .into_iter()
        .fold(HashMap::new(), |mut placed, (before, a)| {
            let waiting: Vec<&str> = items
                .iter()
                .filter(|&&(id, at, low)| {
                    !placed.contains_key(id) && age(at) == a && fits(low, before)
                })
                .map(|&(id, ..)| id)
                .collect();
            let mut started: Vec<&Stat> = tools
                .iter()
                .copied()
                .filter(|s| {
                    ran(s).is_some_and(|r| Some(r) == a.checked_add(before))
                        && !placed.values().any(|p: &&Stat| p.proc.pid == s.proc.pid)
                })
                .collect();
            started.sort_by_key(|s| s.proc.pid);
            let k = waiting.len().min(started.len());
            // Short of shells, those whose output says they ended are the
            // ones left, then the earliest.
            let mut kept: Vec<usize> = (0..waiting.len()).collect();
            kept.sort_by_key(|&i| (!ended.contains(&waiting[i]), i));
            let mut kept = kept.split_off(waiting.len() - k);
            kept.sort_unstable();
            for (i, s) in kept.into_iter().zip(&started[started.len() - k..]) {
                let id = waiting[i];
                placed.insert(id.to_string(), *s);
            }
            placed
        })
}

/// `head` and every process of `tree` under it.
pub fn subtree<'a>(tree: &[&'a Stat], head: &'a Stat) -> Vec<&'a Stat> {
    std::iter::once(head)
        .chain(procs::under(tree, head.proc.pid).into_iter().copied())
        .collect()
}

/// When `s` started, `ps` having said how long it ran by `now`: none when
/// that is more than a time can say, as `ps` can say of a process forked
/// that instant, its run wrapped round.
fn started(now: DateTime<Utc>, s: &Stat) -> Option<DateTime<Utc>> {
    i64::try_from(s.elapsed_secs)
        .ok()
        .and_then(Duration::try_seconds)
        .and_then(|ran| now.checked_sub_signed(ran))
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
    /// What the open call's and its background shells' own output says
    /// each waits on.
    pub said: &'a [Said],
    /// The tool shell each of those is written from ([`placed`]).
    pub placed: &'a HashMap<String, &'a Stat>,
    /// When its processes last used the CPU.
    pub active_at: DateTime<Utc>,
    pub now: DateTime<Utc>,
    /// How long a sleep loop runs idle before it is a wait.
    pub poll: Duration,
}

impl Evidence<'_> {
    /// When `s` started; one whose start can't be said is taken to have
    /// started now, so nothing of it has lasted.
    fn started(&self, s: &Stat) -> DateTime<Utc> {
        started(self.now, s).unwrap_or(self.now)
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
    said_rule(e)
        .or_else(|| lock_rule(e, &below))
        .or_else(|| sleep_rule(e, &below))
        .or_else(|| input_rule(e, &below))
        .or_else(|| watch_rule(e, &below))
        .or_else(|| remote_rule(e, &below))
        .or_else(|| long_rule(e, &below))
        .unwrap_or_else(|| otherwise(e, &below))
}

/// The open call's own wait, else its background shells' longest.
fn said_rule(e: &Evidence<'_>) -> Option<Reading> {
    let own = e
        .open_call
        .and_then(|c| Some((e.said.iter().find(|s| s.call == c.id)?, Some(c))));
    let (said, call) = own.or_else(|| Some((e.said.iter().min_by_key(|s| s.since)?, None)))?;
    Some(Reading {
        kind: Kind::Waits(said.wait),
        words: said.words.clone(),
        since: said.since,
        by: call.map(|c| c.tool.clone()),
        on: None,
        pid: e.placed.get(&said.call).map(|s| s.proc.pid),
        call: Some(said.call.clone()),
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
        let open_call = said.as_ref().map(|_| &call);
        let placed = placed(&tree, open_call, None, &[], &[], now());
        classify(&Evidence {
            tree: &tree,
            open_call,
            said: said.as_slice(),
            placed: &placed,
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

    /// A Claude session's tool shells, each `(pid, ran, cpu)` with a
    /// `cargo build` under it using the same CPU.
    fn shells(each: &[(u32, u64, u64)]) -> Vec<Stat> {
        claude(
            each.iter()
                .flat_map(|&(pid, ran, cpu)| {
                    [
                        Stat {
                            elapsed_secs: ran,
                            ..stat(pid, 100, SHELL)
                        },
                        Stat {
                            elapsed_secs: ran,
                            cpu_secs: cpu,
                            ..stat(pid + 1, pid, "cargo build")
                        },
                    ]
                })
                .collect(),
        )
    }

    #[test]
    fn each_output_is_placed_in_a_shell_of_its_own() {
        let born = |ran: i64| now() - Duration::seconds(ran);
        let all = shells(&[(110, 300, 0), (120, 299, 0), (130, 60, 0)]);
        let tree = work(&all, 100, Engine::Claude, None);
        let call = OpenCall {
            id: "t1".into(),
            tool: "Bash".into(),
            input: String::new(),
            since: born(61),
        };
        let pids = |shells: &[(&str, DateTime<Utc>)], call| {
            let mut pids: Vec<(String, u32)> = placed(&tree, call, None, shells, &[], now())
                .into_iter()
                .map(|(id, s)| (id, s.proc.pid))
                .collect();
            pids.sort();
            pids
        };
        let both = [("b2", born(299)), ("b1", born(300))];
        assert_eq!(
            pids(&both, Some(&call)),
            [("b1".into(), 110), ("b2".into(), 120), ("t1".into(), 130)],
            "launched a second apart, each its own"
        );
        assert_eq!(
            pids(
                &[("b1", born(300)), ("b2", born(299) + Duration::seconds(30))],
                None
            ),
            [("b1".to_string(), 110)],
            "one whose process is gone is placed in none started after it"
        );
        let early = OpenCall {
            since: born(400),
            ..call.clone()
        };
        assert_eq!(
            pids(&both, Some(&early)),
            [("b1".into(), 110), ("b2".into(), 120), ("t1".into(), 130)],
            "the open call never takes a background shell's"
        );
    }

    #[test]
    fn a_shell_in_the_same_second_as_another_is_placed_in_the_one_started_last() {
        let at = |ms: i64| now() - Duration::milliseconds(ms);
        let pids = |all: &[Stat], shells: &[(&str, DateTime<Utc>)], call: Option<&OpenCall>| {
            let tree = work(all, 100, Engine::Claude, None);
            let mut pids: Vec<(String, u32)> = placed(&tree, call, None, shells, &[], now())
                .into_iter()
                .map(|(id, s)| (id, s.proc.pid))
                .collect();
            pids.sort();
            pids
        };
        let call = |since| OpenCall {
            id: "f1".into(),
            tool: "Bash".into(),
            input: String::new(),
            since,
        };
        let same_second = shells(&[(110, 300, 0), (120, 300, 0)]);
        assert_eq!(
            pids(&same_second, &[("b1", at(300_200))], None),
            [("b1".to_string(), 120)],
            "a sub-agent's shell started that second before it is not its own"
        );
        assert_eq!(
            pids(
                &same_second,
                &[("b1", at(300_600)), ("b2", at(300_200))],
                None
            ),
            [("b1".into(), 110), ("b2".into(), 120)],
            "twins of one second, each its own"
        );
        assert_eq!(
            pids(
                &same_second,
                &[("b1", at(300_400))],
                Some(&call(at(300_390)))
            ),
            [("b1".into(), 110), ("f1".into(), 120)],
            "a background call then a foreground one in the same second, each its own"
        );
        let a_second_apart = shells(&[(110, 301, 0), (120, 300, 0)]);
        assert_eq!(
            pids(&a_second_apart, &[("b1", at(300_300))], None),
            [("b1".to_string(), 120)],
            "a sub-agent's shell started a second before its file keeps its own"
        );
        assert_eq!(
            pids(
                &a_second_apart,
                &[("b1", at(300_300))],
                Some(&call(at(301_200)))
            ),
            [("b1".into(), 120), ("f1".into(), 110)],
            "the open call started a second before its file keeps its own"
        );
        let after_its_file = shells(&[(110, 305, 0), (120, 299, 0)]);
        assert_eq!(
            pids(&after_its_file, &[("b2", at(300_002))], None),
            [("b2".to_string(), 120)],
            "its file made a few ms before its shell started"
        );
    }

    #[test]
    fn a_process_whose_run_is_more_than_a_time_can_say_is_never_placed_nor_a_panic() {
        let all = shells(&[(110, u64::MAX, 0), (120, u64::MAX / 2, 0)]);
        let tree = work(&all, 100, Engine::Claude, None);
        let half = all.iter().find(|s| s.proc.pid == 120).unwrap();
        assert!(started(now(), half).is_none());
        let call = bash_call(1);
        let placed = placed(&tree, Some(&call), None, &[("b1", now())], &[], now());
        assert!(placed.is_empty(), "{placed:?}");
        let said = appeared(&call, BUSY, 0);
        let r = classify(&Evidence {
            tree: &tree,
            open_call: Some(&call),
            said: said.as_slice(),
            placed: &placed,
            active_at: now(),
            now: now(),
            poll: Duration::minutes(10),
        });
        assert!(r.since <= now());
    }

    #[test]
    fn twins_of_one_second_one_ended_the_live_one_keeps_its_shell_and_is_never_written_off() {
        use crate::background::{Background, Reported, TaskKind};
        use std::collections::BTreeSet;
        let at = |ms: i64| now() - Duration::milliseconds(ms);
        // b2 then b1 start in the same second; b1 exits at once, its
        // output's last line its end.
        let live = shells(&[(120, 300, 0)]);
        let tree = work(&live, 100, Engine::Claude, None);
        let twins = [("b2", at(300_600)), ("b1", at(300_200))];
        let placed_with = |ended: &[&str]| {
            let mut pids: Vec<(String, u32)> = placed(&tree, None, None, &twins, ended, now())
                .into_iter()
                .map(|(id, s)| (id, s.proc.pid))
                .collect();
            pids.sort();
            pids
        };
        assert_eq!(
            placed_with(&[]),
            [("b1".to_string(), 120)],
            "by birth alone, the one born last"
        );
        assert_eq!(
            placed_with(&["b1"]),
            [("b2".to_string(), 120)],
            "the live one keeps its shell; the ended one is over"
        );
        let found: BTreeSet<String> = placed_with(&["b1"]).into_iter().map(|(id, _)| id).collect();
        let t0 = now();
        let shell = |id| Reported::new(id, TaskKind::Shell);
        let b = Background::default().reported(Some(vec![shell("b2"), shell("b1")]), t0);
        let gone = (1..=4).fold(b, |b, s| {
            b.looked(
                1,
                Some(&found),
                t0 + Duration::seconds(s * 60),
                Duration::zero(),
            )
        });
        assert_eq!(
            gone.shells().collect::<Vec<_>>(),
            ["b2"],
            "the write-off takes the ended one, never the live one"
        );
    }

    #[test]
    fn a_call_waiting_on_approval_is_placed_by_its_own_files_birth_and_one_of_another_tool_in_none()
    {
        let at = |ms: i64| now() - Duration::milliseconds(ms);
        let call = |tool: &str| OpenCall {
            id: "f1".into(),
            tool: tool.into(),
            input: String::new(),
            since: at(305_000),
        };
        // Asked at T0, it waits on the person; a sub-agent's shell S starts
        // at T1, and its own F at T2, its output file born with it.
        let all = shells(&[(110, 303, 0), (120, 300, 0)]);
        let tree = work(&all, 100, Engine::Claude, None);
        let pids = |call: &OpenCall, born| {
            let mut pids: Vec<(String, u32)> = placed(&tree, Some(call), born, &[], &[], now())
                .into_iter()
                .map(|(id, s)| (id, s.proc.pid))
                .collect();
            pids.sort();
            pids
        };
        let bash = call("Bash");
        assert_eq!(
            pids(&bash, Some(at(300_300))),
            [("f1".to_string(), 120)],
            "its own, not the sub-agent's started nearer when it was asked"
        );
        assert_eq!(
            pids(&bash, None),
            [("f1".to_string(), 110)],
            "with no file of its own yet, the first started since it"
        );
        assert!(
            pids(&call("WebFetch"), None).is_empty(),
            "a call of another tool takes no shell"
        );
    }

    #[test]
    fn a_background_shells_held_wait_ends_once_its_own_shell_works_again() {
        let born = |ran: i64| now() - Duration::seconds(ran);
        let before = shells(&[(110, 300, 0), (120, 299, 0)]);
        let prev = runs(&work(&before, 100, Engine::Claude, None));
        let held = said_after(None, "b1", &[BUSY.to_string()], born(300), false);
        let then = |a: u64, b: u64| {
            let all = shells(&[(110, 300, a), (120, 299, b)]);
            let tree = work(&all, 100, Engine::Claude, None);
            let placed = placed(
                &tree,
                None,
                None,
                &[("b1", born(300)), ("b2", born(299))],
                &[],
                now(),
            );
            let busy = placed
                .get("b1")
                .is_some_and(|head| active(&prev, &subtree(&tree, head)));
            said_after(held.clone(), "b1", &[], now(), busy)
        };
        assert!(held.is_some());
        assert_eq!(then(0, 0), held, "its shell still idle");
        assert_eq!(
            then(3, 0),
            None,
            "its own shell works again, its twin launched a second later idle"
        );
        assert_eq!(then(0, 3), held, "its twin working is not its own");
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
            said: said.as_slice(),
            placed: &placed(&tree, Some(&call), None, &[], &[], now()),
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
            placed(&tree, Some(&later), None, &[], &[], now()).get("toolu_1"),
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
    fn the_open_calls_wait_comes_first_then_a_background_shells_longest() {
        let all = idle();
        let tree = work(&all, 100, Engine::Claude, None);
        let call = bash_call(11);
        let shell = |id: &str, line: &str, ago| {
            let at = now() - Duration::minutes(ago);
            said_after(None, id, &[line.to_string()], at, false).unwrap()
        };
        let shells = [shell("b1", "retrying in 30 s", 3), shell("b2", BUSY, 8)];
        let own = [
            shells.to_vec(),
            appeared(&call, "job is queued", 1).into_iter().collect(),
        ]
        .concat();
        let read = |open_call, said: &[Said]| {
            classify(&Evidence {
                tree: &tree,
                open_call,
                said,
                placed: &HashMap::new(),
                active_at: now(),
                now: now(),
                poll: Duration::minutes(10),
            })
        };
        let r = read(Some(&call), &own);
        assert_eq!(
            (r.words.as_str(), r.call.as_deref(), r.by.as_deref()),
            ("queued", Some("toolu_1"), Some("Bash"))
        );
        for open_call in [None, Some(&call)] {
            let r = read(open_call, &shells);
            assert_eq!(r.kind, Kind::Waits(Wait::Queue));
            assert_eq!(r.call.as_deref(), Some("b2"), "the longest-waiting shell");
            assert_eq!(
                (r.since, r.pid, r.by),
                (now() - Duration::minutes(8), None, None)
            );
        }
        assert_eq!(read(None, &[]).kind, Kind::Quiet);
        assert_eq!(read(Some(&call), &[]).kind, Kind::Works);
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
            said: &[],
            placed: &HashMap::new(),
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
