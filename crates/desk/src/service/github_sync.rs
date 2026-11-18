//! A GitHub board's pull and push, as the desk runs them
//! ([`crate::github_sync`] decides): a project is listed lean every pass
//! for its order, and only what changed is read whole; a repository's
//! issues come oldest change first, like Jira's.

use serde_json::{json, Value};

use super::{Budget, Listed, Made};
use crate::board::{self, Board, Sync, System, TicketState};
use crate::board_sync::{Issue, Remote, RemoteError};
use crate::github_sync::{self as gh, Target};
use crate::model::DeskEvent;
use crate::service::{Desk, DeskError};

impl Desk {
    /// What a GitHub board's filter lists, from `since` (the cursor).
    pub(super) async fn github_matching(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        since: Option<&str>,
        budget: &mut Budget,
        out: &mut Listed,
    ) -> Result<(), RemoteError> {
        match gh::target_of(&s.target) {
            Some(Target::Project { .. }) => {
                self.project_matching(s, remote, since, budget, out).await
            }
            Some(Target::Repo { owner, repo }) => {
                self.repo_matching(s, remote, &owner, &repo, since, budget, out)
                    .await
            }
            None => Err(RemoteError::Refused(format!(
                "{} isn't a GitHub project or repository",
                s.target
            ))),
        }
    }

    /// A project: every item, lean, in its own order (each issue's place),
    /// then the ones changed since the cursor read whole, 50 a call.
    async fn project_matching(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        since: Option<&str>,
        budget: &mut Budget,
        out: &mut Listed,
    ) -> Result<(), RemoteError> {
        let project_id = s
            .target_id
            .as_deref()
            .ok_or_else(|| RemoteError::Refused("the link names no GitHub project".into()))?;
        let query = gh::project_query(&s.filter, s.me.as_deref());
        let mut lean: Vec<gh::Lean> = vec![];
        let mut page: Option<String> = None;
        for n in 0.. {
            if n == crate::board_sync::PAGES_PER_PASS || budget.take().is_err() {
                // Only a listing read through says each issue's place and
                // what changed: the next pass lists it again.
                out.complete = false;
                out.ordered = false;
                return Ok(());
            }
            let mut args = json!({ "project_id": project_id });
            if let Some(q) = &query {
                args["query"] = json!(q);
            }
            if let Some(p) = &page {
                args["after"] = json!(p);
            }
            let answer =
                crate::board_sync::call(remote, System::Github, "github.project_items", args)
                    .await?;
            let (items, next, lossy) = gh::lean_items(&answer);
            out.lossy |= lossy;
            lean.extend(items);
            match next {
                Some(next) if page.as_deref() == Some(next.as_str()) => {
                    return Err(RemoteError::Failed(
                        "GitHub gave the same page twice".into(),
                    ))
                }
                Some(next) => page = Some(next),
                None => break,
            }
        }
        // The same issue twice on a project can't be: its first place wins.
        let mut seen = std::collections::HashSet::new();
        lean.retain(|i| seen.insert(i.key.clone()));
        out.places = lean
            .iter()
            .enumerate()
            .map(|(n, i)| (i.key.clone(), i.item_id.clone(), board::github_rank_of(n)))
            .collect();
        let cursor = since
            .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
            .map(|c| c.with_timezone(&chrono::Utc));
        // Read in the project's order, not oldest change first: the cursor
        // goes by the latest change seen, less the look-back.
        out.ordered = false;
        let mut changed: Vec<(usize, &gh::Lean)> = vec![];
        for (n, i) in lean.iter().enumerate() {
            if cursor.is_some_and(|c| i.updated_at < c) {
                continue;
            }
            // At the cursor itself, what its ticket has already seen isn't
            // read again.
            let seen = self
                .0
                .store
                .ticket_by_external(System::Github, s.link, &i.key)
                .await
                .map_err(|e| RemoteError::Failed(e.to_string()))?
                .and_then(|t| t.external)
                .is_some_and(|e| e.synced && e.remote_updated_at >= i.updated_at);
            if !seen {
                changed.push((n, i));
            }
        }
        for chunk in changed.chunks(gh::IDS_PER_READ) {
            if budget.take().is_err() {
                // What was read stands: it is the oldest-changed first only
                // when sorted, so the cursor waits for a full read.
                out.complete = false;
                out.ordered = false;
                return Ok(());
            }
            let ids: Vec<&str> = chunk.iter().map(|(_, i)| i.item_id.as_str()).collect();
            let answer = crate::board_sync::call(
                remote,
                System::Github,
                "github.project_items",
                json!({ "project_id": project_id, "ids": ids, "full": true, "field": gh::STATUS_FIELD }),
            )
            .await?;
            for v in answer["items"].as_array().into_iter().flatten() {
                let Some(id) = v["item_id"].as_str() else {
                    continue;
                };
                let rank = chunk
                    .iter()
                    .find(|(_, i)| i.item_id == id)
                    .map(|(n, _)| board::github_rank_of(*n));
                if let Some(issue) = gh::issue_of(v, &s.states, true, rank) {
                    out.issues.push(issue);
                }
            }
        }
        Ok(())
    }

    /// A repository: its issues changed since the cursor, oldest first; a
    /// whole board that takes the recently done lists the open, then what
    /// was closed lately.
    #[allow(clippy::too_many_arguments)]
    async fn repo_matching(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        owner: &str,
        repo: &str,
        since: Option<&str>,
        budget: &mut Budget,
        out: &mut Listed,
    ) -> Result<(), RemoteError> {
        let mut queries = vec![gh::repo_args(
            owner,
            repo,
            &s.filter,
            s.me.as_deref(),
            since,
            1,
        )];
        if let (board::Filter::All { include_done: true }, None) = (&s.filter, since) {
            queries[0]["state"] = json!("open");
            let mut done = queries[0].clone();
            done["state"] = json!("closed");
            done["since"] = json!(crate::board_sync::done_since(crate::crew::now()).to_rfc3339());
            queries.push(done);
        }
        let mut pages = 0;
        for mut args in queries {
            loop {
                if pages == crate::board_sync::PAGES_PER_PASS || budget.take().is_err() {
                    out.complete = false;
                    return Ok(());
                }
                pages += 1;
                let answer = crate::board_sync::call(
                    remote,
                    System::Github,
                    "github.issues_changed",
                    args.clone(),
                )
                .await?;
                out.issues.extend(
                    answer["issues"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| gh::issue_of(v, &s.states, false, None)),
                );
                match answer["next_page"].as_u64() {
                    Some(n) if n > args["page"].as_u64().unwrap_or(1) => args["page"] = json!(n),
                    _ => break,
                }
            }
        }
        // Two listings: oldest change first only once put together.
        out.issues.sort_by_key(|i| i.updated_at);
        Ok(())
    }

    /// Gives each of board `b`'s tickets its issue's place on the project,
    /// as its listing says (`places`: key, item id, place), unless a drop of
    /// it here is still to go out. Quiet: only the ticket is announced.
    pub(super) async fn take_places(
        &self,
        b: &Board,
        s: &Sync,
        places: &[(String, String, String)],
    ) -> Result<(), DeskError> {
        let by_key: std::collections::HashMap<&str, (&str, &str)> = places
            .iter()
            .map(|(k, id, r)| (k.as_str(), (id.as_str(), r.as_str())))
            .collect();
        let tickets = self
            .0
            .store
            .board_tickets(b.id)
            .await
            .map_err(DeskError::Internal)?;
        for mut t in tickets {
            let Some(e) = t.external.as_mut() else {
                continue;
            };
            if !e.synced || !e.writable_by(s.link) || e.rank_move.is_some() {
                continue;
            }
            let Some((id, rank)) = by_key.get(e.key.as_str()) else {
                continue;
            };
            let mut changed = std::mem::take(&mut e.rank_stale);
            if e.id.as_deref() != Some(*id) {
                e.id = Some(id.to_string());
                changed = true;
            }
            if t.upstream_rank.as_deref() != Some(*rank) {
                t.upstream_rank = Some(rank.to_string());
                changed = true;
            }
            if !changed {
                continue;
            }
            self.save_ticket(&mut t).await?;
            if let Ok(v) = self.view_ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
        Ok(())
    }

    /// The issue `key`, read whole: on a project, its item (by the ticket's
    /// item id), so its Status comes too; else by its repository. `None`
    /// when it is gone or no longer this board's.
    pub(super) async fn github_fetch(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        key: &str,
    ) -> Result<Option<Issue>, RemoteError> {
        let project = gh::target_of(&s.target).is_some_and(|t| t.is_project());
        if project {
            let item = self
                .0
                .store
                .ticket_by_external(System::Github, s.link, key)
                .await
                .map_err(|e| RemoteError::Failed(e.to_string()))?
                .and_then(|t| t.external.and_then(|e| e.id));
            let Some(item) = item else {
                return Ok(None);
            };
            let answer = crate::board_sync::call(
                remote,
                System::Github,
                "github.project_items",
                json!({ "project_id": s.target_id.clone().unwrap_or_default(), "ids": [item], "full": true, "field": gh::STATUS_FIELD }),
            )
            .await?;
            return Ok(answer["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|v| gh::issue_of(v, &s.states, true, None))
                .filter(|i| i.key == key));
        }
        let answer = crate::board_sync::call(
            remote,
            System::Github,
            "github.issues_changed",
            gh::read_args(key),
        )
        .await?;
        Ok(github_read_one(s, &answer).filter(|i| gh::key_in_target(&i.key, &s.target)))
    }

    /// Moves `key`'s issue there to the column `to` maps to, from the one
    /// it was last seen in.
    pub(super) async fn github_move(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        e: &board::External,
        column: TicketState,
    ) -> Result<(), RemoteError> {
        // Done and Dropped close it there even when no column of its maps
        // to them.
        let to = s.states.get(&column);
        let from = e.seen.state.and_then(|st| s.states.get(&st));
        let was_closed = matches!(e.seen.state, Some(TicketState::Done | TicketState::Dropped));
        for (tool, args) in gh::move_calls(
            s,
            &e.key,
            e.id.as_deref(),
            column,
            to.map(String::as_str),
            from.map(String::as_str),
            was_closed,
        ) {
            budget.take()?;
            crate::board_sync::call(remote, System::Github, tool, args).await?;
        }
        Ok(())
    }

    /// Makes ticket `title`/`body` an issue in the board's repository (a
    /// project's own for new tickets), labelled with its Desk ref, and puts
    /// it on the project: its key, its item id there, and its address.
    pub(super) async fn github_create(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        reference: &str,
        title: &str,
        body: &str,
    ) -> Result<(String, Option<String>, String), RemoteError> {
        let repo = new_issues_repo(s).ok_or_else(|| {
            RemoteError::Stuck("Pick a repository for new tickets in the board's link.".into())
        })?;
        let (owner, name) = repo.split_once('/').expect("checked as owner/repo");
        let mut labels = vec![reference.to_string()];
        if let board::Filter::Label(l) = &s.filter {
            labels.push(l.trim().to_string());
        }
        let made = crate::board_sync::call(
            remote,
            System::Github,
            "github.create_issue",
            json!({ "owner": owner, "repo": name, "title": title, "body": body, "labels": labels }),
        )
        .await?;
        let key = made["number"]
            .as_u64()
            .and_then(|n| gh::key_of(&repo, n))
            .ok_or_else(|| RemoteError::Failed("GitHub gave no number for the new issue".into()))?;
        let url = made["html_url"].as_str().unwrap_or_default().to_string();
        let item = self.github_add(s, remote, budget, &key).await?;
        Ok((key, item, url))
    }

    /// Puts issue `key` on the board's project, when it is a project's: its
    /// item id there. Adding one already there answers its item.
    pub(super) async fn github_add(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        key: &str,
    ) -> Result<Option<String>, RemoteError> {
        if !gh::target_of(&s.target).is_some_and(|t| t.is_project()) {
            return Ok(None);
        }
        budget.take()?;
        let mut args = gh::read_args(key);
        args["project_id"] = json!(s.target_id.clone().unwrap_or_default());
        let added =
            crate::board_sync::call(remote, System::Github, "github.project_item_add", args)
                .await?;
        Ok(added["item_id"].as_str().map(str::to_string))
    }

    /// What a create may have made already: an issue in the board's
    /// repository with its Desk ref as a label.
    pub(super) async fn github_find_made(
        &self,
        s: &Sync,
        remote: &dyn Remote,
        budget: &mut Budget,
        reference: &str,
    ) -> Result<Made, RemoteError> {
        let Some(repo) = new_issues_repo(s) else {
            return Ok(Made::Nothing);
        };
        let (owner, name) = repo.split_once('/').expect("checked as owner/repo");
        let answer = crate::board_sync::call(
            remote,
            System::Github,
            "github.issues_changed",
            json!({ "owner": owner, "repo": name, "labels": reference, "state": "all", "per_page": 5 }),
        )
        .await?;
        let Some(issue) = answer["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|v| gh::issue_of(v, &s.states, false, None))
        else {
            return Ok(Made::Nothing);
        };
        // Made, but maybe not yet on the project.
        let item = self.github_add(s, remote, budget, &issue.key).await?;
        Ok(Made::Here((issue.key, item, issue.url)))
    }
}

/// The repository a ticket made here becomes an issue in: a repository
/// board's own, a project's chosen one.
pub(super) fn new_issues_repo(s: &Sync) -> Option<String> {
    match gh::target_of(&s.target)? {
        Target::Repo { owner, repo } => Some(format!("{owner}/{repo}").to_ascii_lowercase()),
        Target::Project { .. } => s.repo.clone().filter(|r| gh::check_repo(r).is_ok()),
    }
}

/// The one issue a `github.issues_changed` read by number answered.
pub(super) fn github_read_one(s: &Sync, answer: &Value) -> Option<Issue> {
    let project = gh::target_of(&s.target).is_some_and(|t| t.is_project());
    answer["issues"]
        .as_array()?
        .first()
        .and_then(|v| gh::issue_of(v, &s.states, project && v.get("status").is_some(), None))
}
