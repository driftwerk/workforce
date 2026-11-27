//! Whether what a session did or was given may carry outside content, as
//! the agents Desk shares its work with are told it: the steps and commands
//! of an exchange that read other people's words, and the tickets whose
//! words aren't all the person's. Pure.

use serde_json::Value;

use crate::board::{Board, Ticket};
use crate::crew::PartyKind;
use crate::ticket_comments::Comment;

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
const GH_READS: [&[&str]; 6] = [
    &["issue"],
    &["pr", "view"],
    &["pr", "diff"],
    &["api"],
    &["release", "view"],
    &["gist"],
];

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
/// fetching command anywhere in it, or `gh` reading issues, pull requests,
/// releases, gists or its API.
fn networked(command: &str) -> bool {
    command
        .split(['|', ';', '&', '(', ')', '`', '\n'])
        .any(|part| {
            let words: Vec<&str> = part
                .split_whitespace()
                .map(|word| word.rsplit('/').next().unwrap_or(word))
                .collect();
            words.iter().enumerate().any(|(at, word)| {
                NETWORK_COMMANDS.contains(word) || (*word == "gh" && gh_reads(&words[at + 1..]))
            })
        })
}

fn gh_reads(args: &[&str]) -> bool {
    let asked: Vec<&str> = args
        .iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    GH_READS.iter().any(|read| asked.starts_with(read))
}

/// Whether `kind` writes the person's own words here: the person, their
/// bosses, Desk.
fn own(kind: &PartyKind) -> bool {
    matches!(kind, PartyKind::You | PartyKind::Manager | PartyKind::Desk)
}

/// Why ticket `t` of `b`, as its session is given it, may carry outside
/// content; none when its words are all the person's or their bosses':
/// one made here by one of them, or an issue synced from where the person
/// is known that they wrote, with every one of its `comments` (as kept)
/// theirs and none unread.
pub fn ticket_outside(t: &Ticket, b: &Board, comments: &[Comment]) -> Option<String> {
    let key = t.key(b).unwrap_or_else(|| "The ticket".into());
    let Some(external) = &t.external else {
        return (!own(&t.created_by.kind))
            .then(|| format!("{key}, which {} wrote", t.created_by.name));
    };
    let me = b.sync.as_ref().and_then(|s| s.me.as_deref());
    let mine = |id: Option<&str>| me.is_some() && id == me;
    let facts = &external.details.facts;
    let read = comments.iter().filter(|c| c.remote_id.is_some()).count();
    let theirs = mine(facts.reporter_id.as_deref())
        && facts.comments.is_some_and(|count| count as usize == read)
        && comments.iter().all(|c| match &c.posted_by {
            Some(by) => own(&by.kind),
            None => mine(c.author.id.as_deref()),
        });
    (!theirs).then(|| {
        format!(
            "{key}, a {} issue not all the person's words",
            external.system.title()
        )
    })
}

#[cfg(test)]
#[path = "knows_outside_tests.rs"]
mod tests;
