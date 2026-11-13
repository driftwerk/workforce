//! A ticket's development, read when it is opened: its issue's pull
//! requests, branches and commits (Jira's `jira.dev_status`, Linear's PR
//! attachments) and its web links (`jira.list_remote_links`, Linear's other
//! link attachments). Pure: the service reads and keeps them.
//!
//! Every word in them is someone else's and comes wrapped: it is unwrapped,
//! put on one line and clipped. Only https addresses are kept.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::board::{Checks, Commits, Dev, DevBranch, DevPr, External, RemoteLink};
use crate::board_sync::{linear_pr, pr_number, pr_state, remote_time, short};
use crate::ticket_files::is_https;

/// The most PRs, branches or links kept on a ticket.
pub const LISTED_MAX: usize = 50;
/// The longest address kept; a longer one is dropped with what it names.
const URL_MAX: usize = 2048;

/// The PRs, branches and commits one read said, kept together: a read
/// that failed leaves all three as they were.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Work {
    pub prs: Vec<DevPr>,
    pub branches: Vec<DevBranch>,
    pub commits: Option<Commits>,
}

/// `url` when it is https and not too long.
fn https(v: &Value) -> Option<String> {
    let url = v.as_str()?.trim();
    (url.len() <= URL_MAX && is_https(url)).then(|| url.to_string())
}

/// The host `url` is on, without `www.`.
fn host_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_string())
}

fn count(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

// ── Jira ─────────────────────────────────────────────────────────────────

/// The tool and arguments that read a Jira issue's development: its id
/// when the pull kept it, which saves looking it up by key.
pub fn dev_status_call(e: &External) -> (&'static str, Value) {
    let mut args = json!({ "issue_key": e.key });
    if let Some(id) =
        e.id.as_deref()
            .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
    {
        args["issue_id"] = json!(id);
    }
    ("jira.dev_status", args)
}

/// The tool and arguments that read a Jira issue's web links.
pub fn remote_links_call(e: &External) -> (&'static str, Value) {
    ("jira.list_remote_links", json!({ "issue_key": e.key }))
}

/// How a build's runs on a branch went: the latest run of each pipeline
/// that wasn't cancelled, counted. `None` when none says.
fn checks_of(branch: &str, builds: &[Value]) -> Option<Checks> {
    let mut runs: Vec<&Value> = builds
        .iter()
        .filter(|b| short(&b["branch"], 200).as_deref() == Some(branch))
        .filter(|b| !matches!(b["state"].as_str(), Some("cancelled" | "unknown") | None))
        .collect();
    runs.sort_by_key(|b| std::cmp::Reverse(remote_time(&b["updated"])));
    let mut seen: Vec<String> = vec![];
    let (mut passed, mut total, mut failed, mut running) = (0u32, 0u32, false, false);
    for b in runs {
        let pipeline = b["pipeline"]
            .as_str()
            .map(str::to_string)
            .or_else(|| short(&b["name"], 200))
            .unwrap_or_default();
        if seen.contains(&pipeline) {
            continue;
        }
        seen.push(pipeline);
        match b["state"].as_str().unwrap_or_default() {
            "successful" => passed += 1,
            "failed" => failed = true,
            "in_progress" | "pending" => running = true,
            _ => continue,
        }
        total += 1;
    }
    (total > 0).then(|| Checks {
        passed,
        total,
        state: match (failed, running) {
            (true, _) => "fail",
            (false, true) => "run",
            _ => "pass",
        }
        .to_string(),
    })
}

/// What a `jira.dev_status` answer says: its PRs (an address that isn't
/// https drops the PR), branches, commits, and each PR's checks when a
/// build on its branch says how they went. `None` when it isn't one.
pub fn jira_work(answer: &Value) -> Option<Work> {
    let builds = answer["builds"].as_array().map_or(&[][..], Vec::as_slice);
    let prs = answer["prs"]
        .as_array()?
        .iter()
        .filter_map(|p| {
            let url = https(&p["url"])?;
            let mut state = pr_state(p["status"].as_str().unwrap_or_default());
            if state == "open" && count(&p["reviewers"]).unwrap_or(0) > 0 {
                state = "review".into();
            }
            Some(DevPr {
                number: count(&p["number"]).or_else(|| pr_number(&url)),
                title: short(&p["title"], 200).unwrap_or_else(|| url.clone()),
                checks: short(&p["source_branch"], 200).and_then(|b| checks_of(&b, builds)),
                author: short(&p["author"], 80),
                repo: short(&p["repo"], 100),
                updated: remote_time(&p["last_update"]),
                state,
                url,
            })
        })
        .take(LISTED_MAX)
        .collect();
    let branches = answer["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(DevBranch {
                name: short(&b["name"], 200)?,
                url: https(&b["url"]),
                repo: short(&b["repo"], 100),
            })
        })
        .take(LISTED_MAX)
        .collect();
    let commits = count(&answer["commits"]["count"]).map(|count| Commits {
        count,
        latest: remote_time(&answer["commits"]["latest"]),
    });
    Some(Work {
        prs,
        branches,
        commits,
    })
}

/// What a `jira.list_remote_links` answer says: each https link, its
/// title (else its address), its app (else its host) and its relation.
/// `None` when it isn't one.
pub fn jira_links(answer: &Value) -> Option<Vec<RemoteLink>> {
    Some(
        answer["links"]
            .as_array()?
            .iter()
            .filter_map(|l| {
                let url = https(&l["url"])?;
                Some(RemoteLink {
                    title: short(&l["title"], 200).unwrap_or_else(|| url.clone()),
                    app: short(&l["app"], 60).or_else(|| host_of(&url)),
                    words: short(&l["relationship"], 60),
                    url,
                })
            })
            .take(LISTED_MAX)
            .collect(),
    )
}

// ── Linear ───────────────────────────────────────────────────────────────

/// A Linear integration's name as an app (`figma` → Figma); none for the
/// generic ones.
fn linear_app(source: &str) -> Option<String> {
    if source.is_empty()
        || source.len() > 30
        || !source.chars().all(|c| c.is_ascii_alphabetic())
        || matches!(source, "api" | "url" | "unknown")
    {
        return None;
    }
    let mut c = source.chars();
    let first = c.next()?.to_ascii_uppercase();
    Some(std::iter::once(first).chain(c).collect())
}

/// What a Linear issue's attachments say of its development: its pull and
/// merge requests (state, number, title, author and repo from the
/// integration's metadata), and its other web links. Uploads are files,
/// not links. `None` when the answer has no attachments at all, so an
/// older hub's leaves what is known.
pub fn linear_dev(issue: &Value) -> Option<(Work, Vec<RemoteLink>)> {
    let nodes = issue.get("attachments")?["nodes"].as_array()?;
    let mut prs = vec![];
    let mut links = vec![];
    for a in nodes {
        let Some(url) = https(&a["url"]) else {
            continue;
        };
        if let Some((_, state)) = linear_pr(a) {
            let m = &a["metadata"];
            let repo = match (short(&m["repoLogin"], 60), short(&m["repoName"], 60)) {
                (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
                (None, Some(name)) => Some(name),
                _ => None,
            };
            prs.push(DevPr {
                number: count(&m["number"]).or_else(|| pr_number(&url)),
                title: short(&m["title"], 200)
                    .or_else(|| short(&a["title"], 200))
                    .unwrap_or_else(|| url.clone()),
                state,
                // Checks only when the metadata says them: it doesn't.
                checks: None,
                author: short(&m["userLogin"], 80),
                repo,
                updated: remote_time(&m["updatedAt"]),
                url,
            });
        } else if !crate::ticket_files::is_linear_upload(&url) {
            links.push(RemoteLink {
                title: short(&a["title"], 200).unwrap_or_else(|| url.clone()),
                app: linear_app(a["sourceType"].as_str().unwrap_or_default())
                    .or_else(|| host_of(&url)),
                words: None,
                url,
            });
        }
    }
    prs.truncate(LISTED_MAX);
    links.truncate(LISTED_MAX);
    Some((
        Work {
            prs,
            branches: vec![],
            commits: None,
        },
        links,
    ))
}

// ── Kept ─────────────────────────────────────────────────────────────────

/// A ticket's development after a read at `at`: each part read replaces
/// what was kept of it, and a part that wasn't read (a hub without the
/// tool, a failed call) is left as it was. With nothing read, it is left
/// whole, so it stays absent until something is.
pub fn merged(
    was: Option<Dev>,
    work: Option<Work>,
    links: Option<Vec<RemoteLink>>,
    at: DateTime<Utc>,
) -> Option<Dev> {
    if work.is_none() && links.is_none() {
        return was;
    }
    let mut dev = was.unwrap_or(Dev {
        prs: vec![],
        branches: vec![],
        commits: None,
        links: vec![],
        read_at: at,
    });
    if let Some(w) = work {
        dev.prs = w.prs;
        dev.branches = w.branches;
        dev.commits = w.commits;
    }
    if let Some(l) = links {
        dev.links = l;
    }
    dev.read_at = at;
    Some(dev)
}

#[cfg(test)]
#[path = "ticket_dev_tests.rs"]
mod tests;
