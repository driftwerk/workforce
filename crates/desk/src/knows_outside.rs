//! Whether what a session did or was given may carry outside content, as
//! the agents Desk shares its work with are told it: the steps and commands
//! of an exchange that read other people's words, and the tickets whose
//! words aren't all the person's. Pure.

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use crate::board::{Board, Ticket};
use crate::crew::PartyKind;
use crate::ticket_comments::{AuthorKind, Comment};
use crate::ticket_history::{HistoryBy, HistoryEvent, HistoryKind};

/// The steps an engine reads the web or hands work on with, lowercased:
/// Claude Code's, Codex's and Gemini's web tools and Claude Code's
/// sub-agent. Any engine's MCP tool is named `<server>__<tool>`.
const OUTSIDE_STEPS: [&str; 7] = [
    "webfetch",
    "web_fetch",
    "websearch",
    "web_search",
    "google_web_search",
    "task",
    "agent",
];

/// The commands that fetch from the network, whatever they are asked.
const NETWORK_COMMANDS: [&str; 8] = [
    "curl", "wget", "http", "https", "xh", "nc", "ncat", "netcat",
];

/// The `gh` subcommands that read what other people wrote.
const GH_READS: [&[&str]; 11] = [
    &["issue"],
    &["pr", "view"],
    &["pr", "diff"],
    &["pr", "list"],
    &["pr", "checkout"],
    &["api"],
    &["release", "view"],
    &["gist"],
    &["search"],
    &["repo", "view"],
    &["run", "view"],
];

/// The `gh` flags whose value is the next word.
const GH_VALUED: [&str; 3] = ["-R", "--repo", "--hostname"];

/// The most characters of a command named as reading outside content.
const SAID_CHARS: usize = 120;

/// The steps and commands of an exchange that read outside content, each
/// once, over all of them: `steps` as the exchange keeps them
/// (`[{tool, input}]`), `commands` its shell commands.
pub fn read_outside(steps: &Value, commands: &[String]) -> Vec<String> {
    let tools = steps
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|step| step.get("tool").and_then(Value::as_str))
        .filter(|tool| outside_step(tool))
        .map(str::to_string);
    let commands = commands
        .iter()
        .filter(|command| networked(command))
        .map(|command| crate::crew::one_line(command, SAID_CHARS));
    tools.chain(commands).fold(Vec::new(), |mut said, read| {
        if !said.contains(&read) {
            said.push(read);
        }
        said
    })
}

fn outside_step(tool: &str) -> bool {
    OUTSIDE_STEPS.contains(&tool.to_ascii_lowercase().as_str()) || tool.contains("__")
}

/// Whether `command` reaches the network for what someone else wrote: a
/// fetching command anywhere in it (quoted too, as `bash -c "curl …"`
/// gives it), or `gh` reading issues, pull requests, releases, gists,
/// searches, repositories, runs or its API.
fn networked(command: &str) -> bool {
    command
        .split(['|', ';', '&', '(', ')', '`', '\n'])
        .any(|part| {
            let words: Vec<&str> = part
                .split_whitespace()
                .map(|word| word.trim_matches(['"', '\'']))
                .collect();
            words.iter().enumerate().any(|(at, word)| {
                let program = word.rsplit('/').next().unwrap_or(word);
                NETWORK_COMMANDS.contains(&program)
                    || (program == "gh" && gh_reads(&words[at + 1..]))
            })
        })
}

/// Whether `gh`'s `args` ask for a read of [`GH_READS`]: its words past
/// every flag and the value of each flag that takes one.
fn gh_reads(args: &[&str]) -> bool {
    let asked: Vec<&str> = args
        .iter()
        .scan(false, |valued, arg| {
            let value = std::mem::replace(valued, GH_VALUED.contains(arg));
            Some((!value && !arg.starts_with('-')).then_some(*arg))
        })
        .flatten()
        .collect();
    GH_READS.iter().any(|read| asked.starts_with(read))
}

/// Whether `kind` writes the person's own words here: the person, their
/// bosses, Desk.
fn own(kind: &PartyKind) -> bool {
    matches!(kind, PartyKind::You | PartyKind::Manager | PartyKind::Desk)
}

/// Who wrote a ticket's words, as Desk keeps them: its comments, its
/// issue's history as last read (none: never read), and what the boss
/// turns that wrote it or its comments had read
/// ([`crate::store::DeskStore::written_outside`]).
#[derive(Debug, Clone, Copy)]
pub struct Writers<'a> {
    pub comments: &'a [Comment],
    pub history: Option<&'a [HistoryEvent]>,
    pub stamps: &'a HashMap<Uuid, Option<String>>,
}

impl Writers<'_> {
    /// Why words `id` that `kind` wrote here may carry outside content:
    /// another's than the person's, their boss's or Desk's; a boss's turn
    /// that had read some; or a boss's from before Desk said.
    fn here(&self, id: Uuid, kind: &PartyKind) -> Option<String> {
        match (self.stamps.get(&id), kind) {
            (Some(Some(read)), _) => Some(format!("written by a boss's turn that had read {read}")),
            (_, k) if !own(k) => Some("not the person's words".into()),
            (None, PartyKind::Manager) => {
                Some("a boss's words from before Desk said what it had read".into())
            }
            _ => None,
        }
    }
}

/// Why ticket `t` of `b`, as its session is given it, may carry outside
/// content; none when its words are all the person's or their bosses'
/// (whose turns had read none): one made here by one of them, its
/// bosses' comments too, or an issue synced from where the person is
/// known that they wrote, with every one of its comments (as kept) theirs
/// and none unread, and no description edit of another's in its history.
pub fn ticket_outside(t: &Ticket, b: &Board, w: Writers<'_>) -> Option<String> {
    let key = t.key(b).unwrap_or_else(|| "The ticket".into());
    // The comments posted here: on a ticket of Desk's own, its bosses'
    // (a session's note on its own ticket is its work, not its brief).
    let posted = |all: bool| {
        w.comments.iter().find_map(|c| {
            let by = c.posted_by.as_ref().filter(|by| all || own(&by.kind))?;
            w.here(c.id, &by.kind)
                .map(|why| format!("{key}, with a comment {why}"))
        })
    };
    let Some(external) = &t.external else {
        return w
            .here(t.id, &t.created_by.kind)
            .map(|why| match own(&t.created_by.kind) {
                true => format!("{key}, {why}"),
                false => format!("{key}, which {} wrote", t.created_by.name),
            })
            .or_else(|| posted(false));
    };
    let me = b.sync.as_ref().and_then(|s| s.me.as_deref());
    let mine = |a: &crate::ticket_comments::Author| {
        a.kind == AuthorKind::Person && me.is_some() && a.id.as_deref() == me
    };
    let facts = &external.details.facts;
    let read = w.comments.iter().filter(|c| c.remote_id.is_some()).count();
    let edited_by_another = w.history.into_iter().flatten().any(|e| {
        e.kind == HistoryKind::Field
            && e.field.as_deref() == Some("description")
            && e.by != HistoryBy::Desk
    });
    let theirs = me.is_some()
        && facts.reporter_id.as_deref() == me
        && facts.comments.is_some_and(|count| count as usize == read)
        && !edited_by_another
        && w.comments.iter().all(|c| match &c.posted_by {
            Some(_) => true,
            None => mine(&c.author),
        });
    match theirs {
        true => posted(true),
        false => Some(format!(
            "{key}, a {} issue not all the person's words",
            external.system.title()
        )),
    }
}

#[cfg(test)]
#[path = "knows_outside_tests.rs"]
mod tests;
