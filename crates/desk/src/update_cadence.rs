//! When a boss tells the person what happened (`[oversight.updates]`'s
//! cadence, [`Cadence`]): each thing it would say is one of five kinds of
//! event ([`EventKind`]), and the person's cadence routes each right away
//! (a message of its own, shape A), into the next roundup (one message an
//! interval, shape C), or to be said only when they ask.
//!
//! - **Quiet hours** hold what would come right away for the morning's
//!   roundup, except what needs the person when they said so.
//! - **Decisions** are told by the person's choice: the important ones
//!   ([`Important`]: it merged, released, deleted, spent, overrode a
//!   session, or set something to happen later) in the roundup, right away,
//!   every one, or only on asking. The rest are routine.
//! - **A roundup** comes on the clock (every 30 minutes, hour or two hours,
//!   or at morning, lunch and end of day), never in the quiet hours, and is
//!   skipped when it would say nothing ([`Roundup::is_empty`]).

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use workforce_common::oversight::{Cadence, Decided, RoundupEvery, Route};

use crate::update::BriefItem;
use crate::update_write::{EventKind, Said};

/// Why a decision is important enough to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Important {
    Merged,
    Released,
    Deleted,
    Spent,
    /// It overrode a session: stopped, interrupted, refused, changed its
    /// mode, or closed it.
    Overrode,
    /// It set something to happen later ("merge it once CI is green").
    Later,
}

impl Important {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Released => "released",
            Self::Deleted => "deleted",
            Self::Spent => "spent",
            Self::Overrode => "overrode",
            Self::Later => "later",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "merged" => Self::Merged,
            "released" => Self::Released,
            "deleted" => Self::Deleted,
            "spent" => Self::Spent,
            "overrode" => Self::Overrode,
            "later" => Self::Later,
            _ => return None,
        })
    }
}

/// What a crew action was, as far as its importance goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// A message to a session or an agent, or an answer to its question.
    Message,
    /// It allowed a session's request.
    Approve,
    /// It refused a session's request (not a plan sent back for changes).
    Deny,
    /// It stopped, interrupted or closed a session, or changed its mode.
    Override,
    /// Anything else: started, resumed, restarted, assigned, moved a
    /// ticket, took on or let go.
    Other,
}

const MERGE: &[&str] = &["merge", "merged", "merges", "merging"];
const RELEASE: &[&str] = &[
    "release",
    "released",
    "releases",
    "releasing",
    "deploy",
    "deployed",
    "deploys",
    "deploying",
    "ship",
    "ships",
    "shipped",
    "publish",
    "published",
    "promote",
    "promoted",
    "tag",
    "tagged",
];
const DELETE: &[&str] = &[
    "delete", "deleted", "deletes", "deleting", "remove", "removed", "rm", "drop", "dropped",
    "wipe", "wiped", "prune", "pruned", "purge", "purged", "clear", "clears", "cleared",
];
const SPEND: &[&str] = &[
    "spend", "spent", "pay", "paid", "buy", "bought", "purchase", "budget",
];
/// Words that say a message holds the step back rather than takes it,
/// when they come just before the step ("don't merge", "wait for CI
/// before merging", "hold off deploying").
const HOLD: &[&str] = &[
    "wait", "hold", "don't", "dont", "not", "no", "never", "before", "until", "pause", "stop",
    "without", "off",
];
/// Words that make the step word before them a thing named rather than a
/// step taken: "the merge queue", "the merge commit", "the deploy
/// preview", "a promote dry run".
const NAMED: &[&str] = &[
    "queue",
    "commit",
    "commits",
    "review",
    "reviews",
    "conflict",
    "conflicts",
    "request",
    "requests",
    "state",
    "status",
    "preview",
    "dry",
    "plan",
    "notes",
    "step",
    "steps",
    "freeze",
    "window",
    "button",
    "gate",
    "check",
    "checks",
];
/// How many words before a step a hold word holds it back.
const HOLD_REACH: usize = 3;

/// Whether every step `w` says is held back: a hold word in the few words
/// just before it. "Merge it before the freeze" takes the step; "wait for
/// green CI before merging" and "don't merge" hold it.
fn held_back(w: &[String]) -> bool {
    let mut any = false;
    for (i, x) in w.iter().enumerate() {
        if !STEPS.iter().any(|(l, _)| l.contains(&x.as_str())) {
            continue;
        }
        any = true;
        let from = i.saturating_sub(HOLD_REACH);
        if !w[from..i].iter().any(|h| HOLD.contains(&h.as_str())) {
            return false;
        }
    }
    any
}
/// Words that say a step is set to happen later.
const LATER: &[&str] = &[
    "if",
    "once",
    "when",
    "after",
    "tonight",
    "tomorrow",
    "later",
    "schedule",
    "scheduled",
];

/// The words of `text`, in lower case, apostrophes kept.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase().replace('’', "'"))
        .collect()
}

/// Whether `w` holds any of `list` (a two-word entry as a pair).
fn says(w: &[String], list: &[&str]) -> bool {
    list.iter().any(|l| match l.split_once(' ') {
        Some((a, b)) => w.windows(2).any(|p| p[0] == a && p[1] == b),
        None => w.iter().any(|x| x == l),
    })
}

/// The steps, each with the importance it gives.
const STEPS: [(&[&str], Important); 4] = [
    (MERGE, Important::Merged),
    (RELEASE, Important::Released),
    (DELETE, Important::Deleted),
    (SPEND, Important::Spent),
];

/// The first step `w` says (`kubectl delete deploy` deletes), as the
/// importance it gives: merging, releasing, deleting or spending.
fn step(w: &[String]) -> Option<Important> {
    w.iter().find_map(|x| {
        STEPS
            .iter()
            .find(|(list, _)| list.contains(&x.as_str()))
            .map(|(_, i)| *i)
    })
}

/// `w` with each step word that names a thing ("the merge queue") blanked:
/// it takes no step.
fn steps_only(mut w: Vec<String>) -> Vec<String> {
    for i in 0..w.len().saturating_sub(1) {
        if NAMED.contains(&w[i + 1].as_str())
            && STEPS.iter().any(|(l, _)| l.contains(&w[i].as_str()))
        {
            w[i].clear();
        }
    }
    w
}

/// Tools that only read, search or change files: what they touch is named
/// by a path, and a path is never a step ("stack-deploy-tests.sh").
const FILE_TOOLS: &[&str] = &[
    "Edit",
    "MultiEdit",
    "Write",
    "NotebookEdit",
    "Read",
    "Grep",
    "Glob",
    "LS",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
];

/// Programs that only read or check what they are given, whatever its
/// words: `grep deploy`, `cat promote.yml`, `shellcheck deploy.sh`.
const READERS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "less",
    "grep",
    "rg",
    "ls",
    "jq",
    "wc",
    "diff",
    "echo",
    "printf",
    "shellcheck",
    "stat",
    "file",
    "df",
    "du",
    "test",
    "cd",
    "pytest",
    "vitest",
    "jest",
];

/// Subcommands that only read, check, build or test: what they are given
/// (a test filter, `cargo test -p desk deploy`) is never a step.
const READING: &[(&str, &[&str])] = &[
    (
        "git",
        &[
            "grep", "diff", "show", "log", "blame", "status", "fetch", "checkout", "switch",
            "branch",
        ],
    ),
    (
        "cargo",
        &[
            "test", "nextest", "check", "clippy", "build", "bench", "fmt",
        ],
    ),
    ("go", &["test", "vet", "build"]),
    ("npm", &["test"]),
    ("pnpm", &["test", "vitest", "jest", "tsc", "eslint"]),
    ("yarn", &["test", "vitest", "jest", "tsc", "eslint"]),
    (
        "npx",
        &["vitest", "jest", "tsc", "eslint", "prettier", "playwright"],
    ),
];

/// Programs that run the script they are given: its file's name says what
/// it does (`bash scripts/deploy.sh`), unless it is only checked (`-n`) or
/// a script is given inline (`-c`).
const INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "python", "python3", "node"];

/// Wrappers that run the command after them, past their own flags, the
/// values of those named here, and a count or duration (`timeout 600`).
const WRAPPERS: &[&str] = &[
    "sudo", "env", "nohup", "time", "timeout", "nice", "command", "exec", "xargs",
];
const WRAPPER_VALUES: &[&str] = &["-u", "-g", "-n", "-s", "-k"];
/// Runners whose `run` (or, for Python, `-m`) names the program they run.
const RUNNERS: &[&str] = &["uv", "poetry", "pipenv", "pdm"];

/// The words of what `words` runs: past assignments, wrappers and their
/// arguments, a runner's `run` and Python's `-m`.
fn what_runs(words: &[String]) -> &[String] {
    let mut w = words;
    loop {
        w = match w {
            [first, rest @ ..] if first.contains('=') && !first.starts_with('-') => rest,
            [first, rest @ ..] if WRAPPERS.contains(&named_as(first).as_str()) => {
                let mut r = rest;
                while let [flag, more @ ..] = r {
                    r = match flag.as_str() {
                        f if WRAPPER_VALUES.contains(&f) => more.get(1..).unwrap_or_default(),
                        f if f.starts_with('-') => more,
                        f if f.chars().next().is_some_and(|c| c.is_ascii_digit()) => more,
                        _ => break,
                    };
                }
                r
            }
            [first, run, rest @ ..]
                if RUNNERS.contains(&named_as(first).as_str()) && run == "run" =>
            {
                rest
            }
            [first, m, rest @ ..]
                if matches!(named_as(first).as_str(), "python" | "python3") && m == "-m" =>
            {
                rest
            }
            _ => return w,
        };
    }
}

/// Flags whose value is prose (a commit message, a PR's title or body):
/// words in it are never the command's step. A cluster of short flags
/// ending in `m` (`-am`) is one too.
const PROSE: &[&str] = &["--message", "--title", "-t", "--body", "-b", "--notes"];

fn prose(flag: &str) -> bool {
    PROSE.contains(&flag) || (!flag.starts_with("--") && flag.len() > 1 && flag.ends_with('m'))
}

/// Flags that delete what the command is given (`find … -delete`).
const DELETING_FLAGS: &[&str] = &["delete", "prune", "purge"];

/// A word as a step would be named in it: a path or file by its name less
/// its extension (`./deploy.sh` is `deploy`, `stack-deploy-tests.sh` is
/// itself), anything else as it is, in lower case.
fn named_as(word: &str) -> String {
    let base = word.rsplit_once('/').map_or(word, |(_, b)| b);
    let stem = match base.split_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    };
    stem.to_lowercase()
}

/// Whether `word` names a file or folder (`scripts/deploy.sh`,
/// `promote.yml`) rather than saying something.
fn pathy(word: &str) -> bool {
    !word.contains(char::is_whitespace) && (word.contains('/') || word.contains('.'))
}

/// Verbs that only look, at any level of a program's subcommands (`gh
/// release list`, `kubectl get deploy`, `docker compose logs`).
const LOOKS: &[&str] = &[
    "list", "ls", "view", "show", "status", "checks", "diff", "watch", "get", "describe", "logs",
    "top", "explain", "ps", "inspect", "images", "download",
];

/// Flags whose value is what a command works on or where (`-n prod`,
/// `--context prod`, `-U postgres`), never what it does.
const VALUES: &[&str] = &[
    "-n",
    "--namespace",
    "--context",
    "-H",
    "-R",
    "--repo",
    "-C",
    "-U",
    "-h",
    "-p",
];

/// `ssh` flags that take a value before its host.
const SSH_VALUES: &[&str] = &[
    "-i", "-p", "-l", "-o", "-J", "-F", "-E", "-b", "-c", "-L", "-R", "-D",
];

/// Database shells, and their flags whose value is a query to run.
const SQL: &[&str] = &["psql", "mysql", "sqlite3"];
const SQL_SCRIPT: &[&str] = &["-c", "--command", "-e", "--execute"];

/// How many of a command's words after its program name what it does
/// (`gh pr merge`, `docker stack deploy`): the rest are what it does it to.
const SUBCOMMANDS: usize = 2;

/// Words in a name that say it only tests or checks
/// (`deploy-preview-tests.sh`, `release:dry-run`): such a name takes no
/// step, whatever else it says.
const CHECKING: &[&str] = &[
    "test", "tests", "spec", "specs", "check", "checks", "lint", "dry", "preview", "plan", "notes",
    "status", "logs", "list", "view", "diff",
];

/// A target runner's flags whose value is where or what it runs in (`pnpm
/// --filter deploy-web build`, `make -f deploy.mk`), never its target.
const TARGET_VALUES: &[&str] = &[
    "--filter",
    "-F",
    "-w",
    "--workspace",
    "--prefix",
    "--dir",
    "-f",
    "--file",
];

/// Programs whose first words name the target or script they run (`make
/// deploy-prod`, `npm run deploy:prod`, `npx release-it`).
const TARGETS: &[&str] = &[
    "make", "just", "task", "rake", "npm", "pnpm", "yarn", "bun", "npx", "pnpx", "bunx",
];

/// The step of the steps `found` says, in order: a deletion wherever it
/// comes (`gh release delete`), else the first.
fn chief(found: impl IntoIterator<Item = Important>) -> Option<Important> {
    let found: Vec<Important> = found.into_iter().collect();
    found
        .iter()
        .copied()
        .find(|i| *i == Important::Deleted)
        .or(found.first().copied())
}

/// The step a name takes by its words, split at `-`, `_`, `:`, `.` and
/// spaces (`deploy-prod`, `deploy:prod`, "Deploy signal"); none when one
/// of them says it only tests or checks ([`CHECKING`]).
fn named_step(name: &str) -> Option<Important> {
    let parts: Vec<String> = name
        .split(|c: char| matches!(c, '-' | '_' | ':' | '.') || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .map(str::to_lowercase)
        .collect();
    match parts.iter().any(|p| CHECKING.contains(&p.as_str())) {
        true => None,
        false => chief(parts.iter().filter_map(|p| step(std::slice::from_ref(p)))),
    }
}

/// The step one command takes (`words`, quotes removed), `depth` commands
/// deep in what hosts and shells run: by its program and its first words
/// ([`SUBCOMMANDS`]). What runs is named by its words ([`named_step`]): the
/// program, the script an interpreter runs, the workflow `gh workflow run`
/// starts, and a target runner's target ([`TARGETS`]); any other word only
/// whole. A path counts only when it is what runs; what a script is given
/// never does. Nothing for a program or subcommand that only reads or
/// looks, for prose, or for what follows `--`.
fn one_step(words: &[String], depth: usize) -> Option<Important> {
    let words = what_runs(words);
    let (first, rest) = words.split_first()?;
    let program = named_as(first);
    if READERS.contains(&program.as_str()) {
        return None;
    }
    // A program that is a step itself (`rm`, `./deploy-prod.sh`) takes it,
    // whatever it is given.
    if let Some(i) = named_step(&program) {
        return Some(i);
    }
    // A command run elsewhere is read as the command it is: on a host
    // (`ssh prod './deploy.sh'`) or by a shell (`bash -c '…'`).
    match program.as_str() {
        "ssh" => {
            let mut r = rest;
            while let [flag, more @ ..] = r {
                r = match flag.as_str() {
                    f if SSH_VALUES.contains(&f) => more.get(1..).unwrap_or_default(),
                    f if f.starts_with('-') => more,
                    _ => break,
                };
            }
            let remote: Vec<&str> = r.iter().skip(1).map(String::as_str).collect();
            return command_step(&remote.join(" "), depth + 1);
        }
        "bash" | "sh" | "zsh" => {
            if let Some(script) = rest.iter().skip_while(|w| *w != "-c").nth(1) {
                return command_step(script, depth + 1);
            }
        }
        _ => {}
    }
    let args: Vec<&String> = rest.iter().take_while(|w| *w != "--").collect();
    let flags = |f: &str| args.iter().any(|w| *w == f);
    let runs_script = INTERPRETERS.contains(&program.as_str()) && !flags("-n") && !flags("-c");
    let sql = SQL.contains(&program.as_str());
    let targets = TARGETS.contains(&program.as_str());
    // How many words more name a target runner's target: its first, and
    // the one after `run` (`npm run deploy:prod`), never a package it
    // installs (`npm install release-it`).
    let mut aim = usize::from(targets);
    let mut said = vec![program.clone()];
    // Which of `said` name what runs, read by their words.
    let mut named: Vec<usize> = vec![0];
    let mut subs = 0;
    let mut skip = false;
    let mut script = false;
    for w in &args {
        // A flag's value is never itself a flag (`git commit -n -m …`).
        if std::mem::take(&mut skip) && !w.starts_with('-') {
            continue;
        }
        // A query's words say what it does, wherever it is given.
        if std::mem::take(&mut script) {
            said.extend(
                w.split_whitespace()
                    .take(SUBCOMMANDS)
                    .map(str::to_lowercase),
            );
            continue;
        }
        let workflow = said.ends_with(&["workflow".to_string(), "run".to_string()]);
        if workflow && !w.starts_with('-') {
            named.push(said.len());
            said.push(named_as(w));
            continue;
        }
        if w.starts_with('-') {
            script = sql && SQL_SCRIPT.contains(&w.as_str());
            skip = prose(w)
                || VALUES.contains(&w.as_str())
                || (targets && TARGET_VALUES.contains(&w.as_str()));
            let flag = w.trim_start_matches('-');
            let flag = flag.split_once('=').map_or(flag, |(f, _)| f);
            if DELETING_FLAGS.contains(&flag) {
                said.push(flag.to_string());
            }
            continue;
        }
        match pathy(w) {
            true if runs_script && subs == 0 => {
                named.push(said.len());
                said.push(named_as(w));
            }
            true => {}
            false if subs < SUBCOMMANDS && !pathy(first) && program != "find" => {
                subs += 1;
                let from = said.len();
                said.extend(
                    w.split_whitespace()
                        .take(SUBCOMMANDS)
                        .map(str::to_lowercase),
                );
                if aim > 0 {
                    aim -= 1;
                    aim += usize::from(matches!(w.as_str(), "run" | "run-script"));
                    named.extend(from..said.len());
                }
            }
            false => {}
        }
    }
    let reads = READING.iter().any(|(p, subs)| {
        *p == program && said.get(1).is_some_and(|sub| subs.contains(&sub.as_str()))
    });
    // `git push -d origin x`, `git push origin :x`.
    if program == "git"
        && said.get(1).is_some_and(|s| s == "push")
        && (flags("-d") || args.iter().any(|w| w.starts_with(':')))
    {
        return Some(Important::Deleted);
    }
    let looks = said[1..].iter().any(|w| LOOKS.contains(&w.as_str()))
        || (program == "git"
            && said.get(1).is_some_and(|s| s == "tag")
            && (flags("-l") || flags("--list")));
    match reads || looks {
        true => None,
        false => chief(
            said.iter()
                .enumerate()
                .filter_map(|(i, w)| match named.contains(&i) {
                    true => named_step(w),
                    false => step(std::slice::from_ref(w)),
                }),
        ),
    }
}

/// How many hosts and shells deep [`command_step`] reads a command.
const RUN_DEPTH: usize = 16;

/// The step a shell command takes: the first any of its commands takes
/// ([`one_step`]); none past [`RUN_DEPTH`].
fn command_step(command: &str, depth: usize) -> Option<Important> {
    if depth > RUN_DEPTH {
        return None;
    }
    crate::computer::boss::commands(command)
        .iter()
        .find_map(|words| one_step(words, depth))
}

/// The step a request a boss allowed takes (`request`: its tool, then
/// what it runs, "Bash: gh pr merge 631"): a command by what it runs, a
/// file tool never, any other tool by its name ("github · merge_pr").
fn approved_step(request: &str) -> Option<Important> {
    let (tool, detail) = request.split_once(": ").unwrap_or((request, ""));
    match tool.trim() {
        t if FILE_TOOLS.contains(&t) => None,
        "Bash" => command_step(detail, 0),
        t => step(&words(&t.replace('_', " "))),
    }
}

/// Whether a decision is important, and why: from what kind of action it
/// was and what it said (`text`: the request it allowed, or the message it
/// sent). Overriding a session always is; allowing a request is when the
/// request merges, releases, deletes or spends (by what it runs, never a
/// file's name); a message is when it tells the session to do one of
/// those, now or later, never when it holds it back ("wait for green CI
/// before merging") or only names a thing ("the merge queue").
pub fn important(kind: ActionKind, text: &str) -> Option<Important> {
    match kind {
        ActionKind::Override | ActionKind::Deny => Some(Important::Overrode),
        ActionKind::Approve => approved_step(text),
        ActionKind::Message => {
            // Each clause on its own: "No, merge it." merges.
            let v = text
                .split([',', '.', ';', ':', '!', '?', '—', '–'])
                .map(|c| steps_only(words(c)))
                .filter(|w| !held_back(w))
                .find_map(|w| step(&w))?;
            Some(match says(&words(text), LATER) {
                true => Important::Later,
                false => v,
            })
        }
        ActionKind::Other => None,
    }
}

/// Where an event of `kind` goes by `c`, `quiet` being whether it is the
/// quiet hours: in them, what would come right away waits for the roundup,
/// except what needs the person when they said so. Needs-you is never only
/// on asking.
pub fn route(kind: EventKind, c: &Cadence, quiet: bool) -> Route {
    let base = match kind {
        EventKind::NeedsYou => match c.needs_you {
            Route::Ask => Route::RightAway,
            r => r,
        },
        EventKind::Problem => c.problems,
        EventKind::Finished => c.finished,
        EventKind::Progress => c.progress,
        EventKind::Routine => c.routine,
    };
    match (base, quiet) {
        (Route::RightAway, true) if !(kind == EventKind::NeedsYou && c.quiet_needs_you) => {
            Route::Roundup
        }
        (r, _) => r,
    }
}

/// Where a decision goes by `c`: an important one by the decisions
/// setting, any other as routine. In the quiet hours, as [`route`].
pub fn decision_route(important: bool, c: &Cadence, quiet: bool) -> Route {
    let r = match (c.decisions, important) {
        (Decided::Ask, _) => Route::Ask,
        (Decided::All, _) => Route::Roundup,
        (Decided::Important, true) => Route::Roundup,
        (Decided::ImportantRightAway, true) => Route::RightAway,
        (_, false) => c.routine,
    };
    // With no roundups, what would wait for one comes right away (in
    // the quiet hours, as they end).
    let r = match (r, c.roundup) {
        (Route::Roundup, RoundupEvery::Never) => Route::RightAway,
        (r, _) => r,
    };
    match (r, quiet) {
        (Route::RightAway, true) => Route::Roundup,
        (r, _) => r,
    }
}

/// Where what one of the person's monitors saw goes by `c`, `quiet` being
/// whether it is the quiet hours. One for their DM is their own "tell me
/// when", so it comes right away; a notice is worth knowing, and goes as
/// progress does. Neither is ever only on asking (a monitor the person set
/// up to tell them says it in the roundup), and with no roundups what would
/// wait for one comes right away. In the quiet hours what would come right
/// away waits for their end: the first roundup after them.
pub fn monitor_route(notice: bool, c: &Cadence, quiet: bool) -> Route {
    let r = match (notice, c.progress) {
        (false, _) => Route::RightAway,
        (true, Route::Ask) => Route::Roundup,
        (true, r) => r,
    };
    let r = match (r, c.roundup) {
        (Route::Roundup, RoundupEvery::Never) => Route::RightAway,
        (r, _) => r,
    };
    match (r, quiet) {
        (Route::RightAway, true) => Route::Roundup,
        (r, _) => r,
    }
}

/// Whether `at` (local time) is in the quiet hours `quiet` (from, until),
/// which may run over midnight: from is in them, until isn't.
pub fn in_quiet(quiet: Option<((u32, u32), (u32, u32))>, at: NaiveTime) -> bool {
    let Some(((fh, fm), (uh, um))) = quiet else {
        return false;
    };
    let m = at.hour() * 60 + at.minute();
    let (from, until) = (fh * 60 + fm, uh * 60 + um);
    match from < until {
        true => (from..until).contains(&m),
        // Over midnight: 22:00–07:00.
        false => m >= from || m < until,
    }
}

/// Whether it is the quiet hours of `c` at `now`, in `tz`.
pub fn quiet_now<Tz: TimeZone>(c: &Cadence, now: DateTime<Utc>, tz: &Tz) -> bool {
    in_quiet(c.quiet_hours, now.with_timezone(tz).time())
}

/// The first roundup time of `c` after `after`, in `tz`: on the clock
/// (every 30 minutes at :00 and :30, every hour at :00, every two hours at
/// even hours) or at morning, lunch and end of day, never in the quiet
/// hours. With roundups never, the end of the quiet hours is the one time
/// a roundup comes: what was held through them is said then, never lost.
/// None only with no roundups and no quiet hours. Each time is a local
/// clock time, so a change of clock neither moves nor skips one; a time
/// the clock jumps over is left out.
pub fn next_roundup<Tz: TimeZone>(
    c: &Cadence,
    after: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    let local = after.with_timezone(tz).date_naive();
    // The local clock times of a day that a roundup comes at.
    let times: Vec<u32> = match c.roundup {
        RoundupEvery::Never => match c.quiet_hours {
            Some((_, (uh, um))) => vec![uh * 60 + um],
            None => return None,
        },
        RoundupEvery::Minutes(m) => {
            let step = m.clamp(30, 120);
            (0..24 * 60 / step).map(|k| k * step).collect()
        }
        RoundupEvery::Meals => RoundupEvery::MEALS
            .iter()
            .map(|(h, m)| h * 60 + m)
            .collect(),
    };
    let never = c.roundup == RoundupEvery::Never;
    // Three days of times is always enough to find one out of quiet hours.
    for day in 0..3 {
        let date = local + Duration::days(day);
        for m in &times {
            let Some(t) = NaiveTime::from_hms_opt(m / 60, m % 60, 0) else {
                continue;
            };
            let Some(at) = tz.from_local_datetime(&date.and_time(t)).earliest() else {
                continue;
            };
            let at = at.with_timezone(&Utc);
            if at > after && (never || !in_quiet(c.quiet_hours, t)) {
                return Some(at);
            }
        }
    }
    None
}

/// Until when what needs the person waits by `c` at `now`, in `tz`: none
/// when it comes right away; else the next roundup, or, with none to come,
/// the end of the quiet hours.
pub fn held_until<Tz: TimeZone>(c: &Cadence, now: DateTime<Utc>, tz: &Tz) -> Option<DateTime<Utc>> {
    if route(EventKind::NeedsYou, c, quiet_now(c, now, tz)) == Route::RightAway {
        return None;
    }
    next_roundup(c, now, tz).or_else(|| {
        let every_half_hour = Cadence {
            roundup: RoundupEvery::Minutes(30),
            ..*c
        };
        next_roundup(&every_half_hour, now, tz)
    })
}

/// Whether a roundup is due at `now`: the first roundup time after the
/// last one (`last`) has come.
pub fn roundup_due<Tz: TimeZone>(
    c: &Cadence,
    last: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> bool {
    next_roundup(c, last, tz).is_some_and(|at| at <= now)
}

/// A roundup's sections, in the order it says them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionKey {
    UpToYou,
    Decided,
    Finished,
    WorthKnowing,
    StillGoing,
}

impl SectionKey {
    pub const ORDER: [Self; 5] = [
        Self::UpToYou,
        Self::Decided,
        Self::Finished,
        Self::WorthKnowing,
        Self::StillGoing,
    ];

    /// Its title, as the roundup says it.
    pub fn title(self) -> &'static str {
        match self {
            Self::UpToYou => "Up to you",
            Self::Decided => "Decided without you",
            Self::Finished => "Finished",
            Self::WorthKnowing => "Worth knowing",
            Self::StillGoing => "Still going",
        }
    }

    /// The section an item of `kind` is said in.
    pub fn of(kind: EventKind) -> Self {
        match kind {
            EventKind::NeedsYou => Self::UpToYou,
            EventKind::Finished => Self::Finished,
            EventKind::Problem => Self::WorthKnowing,
            EventKind::Progress | EventKind::Routine => Self::StillGoing,
        }
    }
}

/// One section of a roundup: its lines, each a full sentence with its
/// session's link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub key: SectionKey,
    pub title: String,
    pub lines: Vec<Said>,
}

/// A roundup (shape C, `extras.update.roundup`): what happened since the
/// last, by what it means to the person, each section only when it says
/// something, and how many routine things it left out ("N routine things
/// · Show"), shown as runs: one line a session and kind of decision, each
/// of them behind it ([`crate::update_routine`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roundup {
    pub since: DateTime<Utc>,
    pub sections: Vec<Section>,
    #[serde(default)]
    pub routine: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<Said>,
}

impl Roundup {
    /// `lines` (each with the section it goes in) as a roundup since
    /// `since`, its sections in order and each only when it has a line;
    /// `routine` things left out, said as `runs`.
    pub fn of(
        since: DateTime<Utc>,
        lines: Vec<(SectionKey, Said)>,
        routine: u32,
        runs: Vec<Said>,
    ) -> Self {
        let sections = SectionKey::ORDER
            .into_iter()
            .map(|key| Section {
                key,
                title: key.title().to_string(),
                lines: lines
                    .iter()
                    .filter(|(k, _)| *k == key)
                    .map(|(_, s)| s.clone())
                    .collect(),
            })
            .filter(|s| !s.lines.is_empty())
            .collect();
        Self {
            since,
            sections,
            routine,
            runs,
        }
    }

    /// Whether it says nothing but routine: then it isn't sent.
    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|s| s.lines.is_empty())
    }

    /// Its one-line headline, written plainly: what is in it, in counts.
    pub fn plain_headline(&self) -> String {
        // What it decided is counted by decision, though a session's are
        // one line.
        let n = |k: SectionKey| {
            self.sections.iter().find(|s| s.key == k).map_or(0, |s| {
                s.lines
                    .iter()
                    .map(|l| l.decisions.map_or(1, |n| n as usize))
                    .sum()
            })
        };
        let mut parts = Vec::new();
        let (fin, up, dec, worth, going) = (
            n(SectionKey::Finished),
            n(SectionKey::UpToYou),
            n(SectionKey::Decided),
            n(SectionKey::WorthKnowing),
            n(SectionKey::StillGoing),
        );
        if fin > 0 {
            parts.push(format!("{fin} finished"));
        }
        if up > 0 {
            parts.push(format!("{up} up to you"));
        }
        if dec > 0 {
            parts.push(format!("{dec} decided without you"));
        }
        if worth > 0 {
            parts.push(format!("{worth} worth knowing"));
        }
        if going > 0 {
            parts.push(format!("{going} still going"));
        }
        match parts.is_empty() {
            true => "Nothing new.".into(),
            false => crate::update_write::capitalised(&format!("{}.", parts.join(", "))),
        }
    }
}

/// How many of a session's decisions its entry of "Decided without you"
/// shows before "N more" (the notable ones are shown however many).
pub const ENTRY_SHOWN: usize = 4;

/// The decisions of an entry (`did`, the newest first) it shows: every
/// notable one, and the newest of the rest up to [`ENTRY_SHOWN`], in
/// order; and how many it leaves behind "N more".
pub fn shown(did: &[BriefItem]) -> (Vec<&BriefItem>, usize) {
    let notable = did.iter().filter(|i| i.notable).count();
    let mut room = ENTRY_SHOWN.saturating_sub(notable);
    let mut shown: Vec<&BriefItem> = Vec::new();
    for i in did {
        if i.notable {
            shown.push(i);
        } else if room > 0 {
            room -= 1;
            shown.push(i);
        }
    }
    let more = did.len() - shown.len();
    (shown, more)
}

/// A roundup as text, for history, Slack and the model: its headline, each
/// section's title and lines (a session's decisions under its line, the
/// rest counted), and the routine count with a line a run.
pub fn render(headline: &str, r: &Roundup) -> String {
    let line = |l: &Said| {
        let mut out = format!("- {}", crate::update_write::render(l).replace('\n', " · "));
        if l.decisions.is_some() {
            let (shown, more) = shown(&l.did);
            for i in shown {
                out.push_str(&match i.why.as_deref().filter(|w| !w.trim().is_empty()) {
                    Some(why) => format!("\n  - {} — {why}", i.action),
                    None => format!("\n  - {}", i.action),
                });
            }
            if more > 0 {
                out.push_str(&format!("\n  - {more} more"));
            }
        }
        out
    };
    let mut out = vec![headline.trim().to_string()];
    for s in &r.sections {
        let lines: Vec<String> = s.lines.iter().map(line).collect();
        out.push(format!("**{}**\n{}", s.title, lines.join("\n")));
    }
    if r.routine > 0 {
        let count = format!(
            "{} routine {}",
            r.routine,
            if r.routine == 1 { "thing" } else { "things" }
        );
        out.push(match r.runs.is_empty() {
            true => format!("{count}: ask \"what else did you decide?\""),
            false => {
                let runs: Vec<String> = r.runs.iter().map(line).collect();
                format!("{count}:\n{}", runs.join("\n"))
            }
        });
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_write::SessionLink;

    fn balanced() -> Cadence {
        Cadence::default()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Each kind goes where the person's cadence sends it; needs-you is
    /// never only on asking.
    #[test]
    fn each_kind_is_routed_as_the_cadence_says() {
        let c = balanced();
        assert_eq!(c.preset_name(), Some("balanced"));
        assert_eq!(route(EventKind::NeedsYou, &c, false), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &c, false), Route::RightAway);
        assert_eq!(route(EventKind::Finished, &c, false), Route::Roundup);
        assert_eq!(route(EventKind::Progress, &c, false), Route::Roundup);
        assert_eq!(route(EventKind::Routine, &c, false), Route::Ask);

        let now = Cadence::preset("as_it_happens").unwrap();
        assert_eq!(route(EventKind::Finished, &now, false), Route::RightAway);
        assert_eq!(route(EventKind::Progress, &now, false), Route::RightAway);
        assert_eq!(route(EventKind::Routine, &now, false), Route::Ask);
        assert_eq!(now.roundup, RoundupEvery::Never);

        let later = Cadence::preset("roundups_only").unwrap();
        assert_eq!(route(EventKind::NeedsYou, &later, false), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &later, false), Route::Roundup);

        let mut custom = c;
        custom.progress = Route::Ask;
        assert_eq!(custom.preset_name(), None, "a changed row is Custom");
        custom.needs_you = Route::Ask;
        assert_eq!(route(EventKind::NeedsYou, &custom, false), Route::RightAway);
    }

    /// A monitor's finding for the DM comes at once, out of the quiet
    /// hours; a notice goes as progress does.
    #[test]
    fn a_monitor_tells_the_dm_at_once_but_not_in_the_quiet_hours_and_a_notice_as_progress() {
        let c = Cadence::default();
        assert_eq!(monitor_route(false, &c, false), Route::RightAway);
        assert_eq!(monitor_route(false, &c, true), Route::Roundup);
        assert_eq!(monitor_route(true, &c, false), Route::Roundup, "Balanced");
        let at_once = Cadence::preset("as_it_happens").unwrap();
        assert_eq!(monitor_route(true, &at_once, false), Route::RightAway);
        assert_eq!(monitor_route(true, &at_once, true), Route::Roundup);
        let asked = Cadence {
            progress: Route::Ask,
            ..c
        };
        assert_eq!(monitor_route(true, &asked, false), Route::Roundup);
        let never = Cadence {
            roundup: RoundupEvery::Never,
            ..c
        };
        assert_eq!(monitor_route(true, &never, false), Route::RightAway);
        // The quiet hours' end is the one roundup a cadence of none has.
        assert_eq!(monitor_route(false, &never, true), Route::Roundup);
        let tz = chrono_tz::Europe::London;
        let at_11pm = tz
            .with_ymd_and_hms(2026, 11, 19, 23, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let next = next_roundup(&never, at_11pm, &tz).unwrap();
        assert_eq!(
            next.with_timezone(&tz).time(),
            NaiveTime::from_hms_opt(7, 0, 0).unwrap()
        );
    }

    /// Quiet hours hold what would come right away for the roundup, except
    /// what needs the person when they said so; they run over midnight.
    #[test]
    fn quiet_hours_hold_all_but_what_needs_you() {
        let c = balanced();
        assert!(in_quiet(c.quiet_hours, t(22, 0)));
        assert!(in_quiet(c.quiet_hours, t(3, 15)));
        assert!(in_quiet(c.quiet_hours, t(6, 59)));
        assert!(!in_quiet(c.quiet_hours, t(7, 0)));
        assert!(!in_quiet(c.quiet_hours, t(21, 59)));
        assert!(!in_quiet(None, t(3, 0)));
        assert!(in_quiet(Some(((12, 0), (13, 0))), t(12, 30)));
        assert!(!in_quiet(Some(((12, 0), (13, 0))), t(13, 0)));

        assert_eq!(route(EventKind::NeedsYou, &c, true), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &c, true), Route::Roundup);
        let mut strict = c;
        strict.quiet_needs_you = false;
        assert_eq!(route(EventKind::NeedsYou, &strict, true), Route::Roundup);
        let mut now = Cadence::preset("as_it_happens").unwrap();
        now.decisions = Decided::ImportantRightAway;
        assert_eq!(decision_route(true, &now, false), Route::RightAway);
        assert_eq!(decision_route(true, &now, true), Route::Roundup);
        // As it happens: an important decision comes at once too, there
        // being no roundup to wait for.
        let plain = Cadence::preset("as_it_happens").unwrap();
        assert_eq!(decision_route(true, &plain, false), Route::RightAway);

        // What needs the person waits only when they said so.
        let utc = chrono_tz::UTC;
        assert_eq!(held_until(&c, at("2026-11-16T23:00:00Z"), &utc), None);
        assert_eq!(
            held_until(&strict, at("2026-11-16T23:00:00Z"), &utc),
            Some(at("2026-11-17T07:00:00Z"))
        );
        let mut never = strict;
        never.roundup = RoundupEvery::Never;
        assert_eq!(
            held_until(&never, at("2026-11-16T23:00:00Z"), &utc),
            Some(at("2026-11-17T07:00:00Z")),
            "no roundups: until the quiet hours end"
        );
        let tz = chrono_tz::Europe::London;
        // 23:00 in London in November: no quiet-hours roundups, the next
        // at 07:00, the first hour out of them.
        assert!(quiet_now(&c, at("2026-11-16T23:00:00Z"), &tz));
        assert_eq!(
            next_roundup(&c, at("2026-11-16T21:40:00Z"), &tz),
            Some(at("2026-11-17T07:00:00Z"))
        );
    }

    /// Which decisions are important: merging, releasing, deleting,
    /// spending, overriding a session, setting something for later; never
    /// a message that holds a step back, nor the routine.
    #[test]
    fn which_decisions_count_as_important() {
        use ActionKind::*;
        assert_eq!(
            important(Override, "stopped web"),
            Some(Important::Overrode)
        );
        assert_eq!(
            important(Deny, "refused the push"),
            Some(Important::Overrode)
        );
        assert_eq!(
            important(Approve, "Bash: gh pr merge 631 --squash"),
            Some(Important::Merged)
        );
        assert_eq!(
            important(Approve, "Bash: rm -rf target/"),
            Some(Important::Deleted)
        );
        assert_eq!(important(Approve, "Bash: cargo test"), None);
        assert_eq!(
            important(Message, "Merge both changes yourself."),
            Some(Important::Merged)
        );
        assert_eq!(
            important(
                Message,
                "Delete rebuildable build files on the devbox if free space falls under 50 GB."
            ),
            Some(Important::Later)
        );
        assert_eq!(
            important(Message, "Wait for green CI before merging."),
            None
        );
        assert_eq!(important(Message, "Don't deploy tonight."), None);
        assert_eq!(important(Message, "Hold off deploying it."), None);
        assert_eq!(important(Message, "Do not merge yet."), None);
        // A hold word elsewhere in the message holds nothing back.
        assert_eq!(
            important(Message, "Merge it before the freeze."),
            Some(Important::Merged)
        );
        assert_eq!(
            important(Message, "Release without the flag."),
            Some(Important::Released)
        );
        assert_eq!(
            important(Message, "Stop the watcher and delete the old build files."),
            Some(Important::Deleted)
        );
        assert_eq!(important(Message, "Run the tests again."), None);
        assert_eq!(important(Other, "restarted web"), None);

        let c = balanced();
        assert_eq!(decision_route(true, &c, false), Route::Roundup);
        assert_eq!(decision_route(false, &c, false), Route::Ask, "routine");
        let mut every = c;
        every.decisions = Decided::Ask;
        assert_eq!(decision_route(true, &every, false), Route::Ask);
    }

    #[test]
    fn roundups_come_on_the_clock() {
        let tz = chrono_tz::UTC;
        let c = balanced();
        assert_eq!(
            next_roundup(&c, at("2026-11-16T09:00:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        assert_eq!(
            next_roundup(&c, at("2026-11-16T09:59:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        assert!(roundup_due(
            &c,
            at("2026-11-16T09:10:00Z"),
            at("2026-11-16T10:00:30Z"),
            &tz
        ));
        assert!(!roundup_due(
            &c,
            at("2026-11-16T09:10:00Z"),
            at("2026-11-16T09:59:30Z"),
            &tz
        ));
        let mut half = c;
        half.roundup = RoundupEvery::Minutes(30);
        assert_eq!(
            next_roundup(&half, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T09:30:00Z"))
        );
        let mut two = c;
        two.roundup = RoundupEvery::Minutes(120);
        assert_eq!(
            next_roundup(&two, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        let mut meals = c;
        meals.roundup = RoundupEvery::Meals;
        assert_eq!(
            next_roundup(&meals, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T12:30:00Z"))
        );
        assert_eq!(
            next_roundup(&meals, at("2026-11-16T17:30:00Z"), &tz),
            Some(at("2026-11-17T09:00:00Z"))
        );
        // No roundups: what the quiet hours held is said as they end, so
        // nothing held is lost; with no quiet hours either, never.
        let mut never = c;
        never.roundup = RoundupEvery::Never;
        assert_eq!(
            next_roundup(&never, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-17T07:00:00Z"))
        );
        never.quiet_hours = None;
        assert_eq!(next_roundup(&never, at("2026-11-16T09:10:00Z"), &tz), None);
    }

    /// A change of clock neither skips nor moves a roundup: each is a
    /// local clock time, and one the clock jumps over is left out.
    #[test]
    fn roundups_keep_to_the_local_clock_over_a_change_of_clock() {
        let london = chrono_tz::Europe::London;
        let mut c = balanced();
        c.quiet_hours = None;
        // 29 Mar 2026: 01:00 GMT becomes 02:00 BST. At 10:10 BST the next
        // hourly roundup is 11:00 BST (10:00Z).
        assert_eq!(
            next_roundup(&c, at("2026-03-29T09:10:00Z"), &london),
            Some(at("2026-03-29T10:00:00Z"))
        );
        // Every two hours: even local hours, 12:00 BST.
        let mut two = c;
        two.roundup = RoundupEvery::Minutes(120);
        assert_eq!(
            next_roundup(&two, at("2026-03-29T09:10:00Z"), &london),
            Some(at("2026-03-29T11:00:00Z"))
        );
        // 00:30 GMT: the 01:00 slot doesn't exist that day; the next is
        // 02:00 BST (01:00Z) — 01:00Z is 02:00 BST.
        assert_eq!(
            next_roundup(&c, at("2026-03-29T00:30:00Z"), &london),
            Some(at("2026-03-29T01:00:00Z"))
        );
        // A zone whose midnight is skipped still has that day's roundups.
        let santiago = chrono_tz::America::Santiago;
        assert!(next_roundup(&c, at("2026-09-06T03:30:00Z"), &santiago).is_some());
        // What needs the person, held for the roundup, is held to a time.
        let mut held = c;
        held.needs_you = Route::Roundup;
        assert!(held_until(&held, at("2026-03-29T00:30:00Z"), &london).is_some());
    }

    fn line(kind: EventKind, headline: &str) -> Said {
        Said {
            kind,
            headline: headline.into(),
            body: String::new(),
            ids: vec![],
            not_checked: None,
            session: Some(SessionLink {
                session_id: "s".into(),
                name: "web".into(),
                closed: false,
            }),
            did: vec![],
            at: None,
            decisions: None,
        }
    }

    /// A roundup says its sections in order, each only when it has a
    /// line; one with only routine things is empty, and not sent.
    #[test]
    fn an_empty_roundup_is_skipped_and_sections_come_in_order() {
        let since = at("2026-11-16T08:00:00Z");
        let routine_only = Roundup::of(since, vec![], 15, vec![]);
        assert!(routine_only.is_empty());
        let r = Roundup::of(
            since,
            vec![
                (
                    SectionKey::StillGoing,
                    line(EventKind::Progress, "Production is steady."),
                ),
                (
                    SectionKey::Finished,
                    line(EventKind::Finished, "The Rust builds work again."),
                ),
                (
                    SectionKey::UpToYou,
                    line(EventKind::NeedsYou, "Cufflink is paused."),
                ),
            ],
            15,
            vec![],
        );
        assert!(!r.is_empty());
        let keys: Vec<SectionKey> = r.sections.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                SectionKey::UpToYou,
                SectionKey::Finished,
                SectionKey::StillGoing
            ]
        );
        assert_eq!(r.sections[0].title, "Up to you");
        assert_eq!(
            r.plain_headline(),
            "1 finished, 1 up to you, 1 still going."
        );
        let text = render("Two fixes are finished.", &r);
        assert!(
            text.contains("**Finished**\n- **The Rust builds work again.**"),
            "{text}"
        );
        assert!(
            text.ends_with("15 routine things: ask \"what else did you decide?\""),
            "{text}"
        );
    }
}
