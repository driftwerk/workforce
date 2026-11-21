//! Bounded-parallel pull across every repo in a project.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::warn;

use workforce_common::redact::redact_secrets;
use workforce_common::text::truncate_with_ellipsis;
use workforce_projects::{Project, RepoSource};

use super::fetcher::{Fetcher, PullOutcome};
use super::layout;

#[derive(Debug, Clone, Serialize)]
pub struct RepoUpdateReport {
    pub alias: String,
    pub branch: String,
    /// `Ok` carries the per-repo `PullOutcome`; `Err` carries the failure
    /// message. Failures don't abort the project-wide update — every repo
    /// is reported on independently.
    #[serde(flatten)]
    pub result: RepoUpdateResult,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RepoUpdateResult {
    Ok(PullOutcome),
    Err { error: String },
}

/// Bound on a failed repo's error in the refresh warning; git's stderr is unbounded.
const LOGGED_ERROR_MAX_BYTES: usize = 2048;

impl RepoUpdateReport {
    /// Why this repo failed, fit for a log: credentials redacted while the
    /// lines are whole, the lines joined so a server's `remote:` text can't
    /// start a log line of its own, and the cut last, so no fragment of a
    /// credential survives it.
    fn logged_error(&self) -> Option<String> {
        match &self.result {
            RepoUpdateResult::Ok(_) => None,
            RepoUpdateResult::Err { error } => {
                let redacted = redact_secrets(error);
                let joined = redacted
                    .split(['\r', '\n'])
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                Some(truncate_with_ellipsis(&joined, LOGGED_ERROR_MAX_BYTES))
            }
        }
    }
}

/// Warn once for each repo of `project` that failed to update, with why.
pub(crate) fn warn_failures(project: &str, reports: &[RepoUpdateReport]) {
    reports
        .iter()
        .filter_map(|r| r.logged_error().map(|error| (r, error)))
        .for_each(|(r, error)| {
            warn!(
                target: "cc",
                project = %project,
                alias = %r.alias,
                branch = %r.branch,
                error = %error,
                "cc project refresh: repo failed; continuing with its stale state"
            )
        });
}

pub struct ProjectUpdater {
    /// Pre-resolved per-owner GitHub PATs. Empty for public-only setups.
    /// Resolution lives at the call site so the `CURRENT_USER_ID`
    /// task-local is still set when `LayeredUserVault::get_github_pat`
    /// runs — per-repo lookup inside spawned tasks would lose it.
    tokens_by_owner: HashMap<String, String>,
    data_dir: PathBuf,
    concurrency: usize,
}

impl ProjectUpdater {
    pub fn new(
        tokens_by_owner: HashMap<String, String>,
        data_dir: PathBuf,
        concurrency: usize,
    ) -> Self {
        Self {
            tokens_by_owner,
            data_dir,
            concurrency: concurrency.max(1),
        }
    }

    /// Pull every repo in `project` to its effective default branch.
    /// Concurrency is bounded by the configured `update_concurrency`; one
    /// failure doesn't abort siblings.
    pub async fn update(&self, project: &Project) -> Result<Vec<RepoUpdateReport>> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let tasks: Vec<_> = project
            .repos
            .iter()
            .map(|repo| {
                let alias = repo.alias.clone();
                let branch = project.effective_default_branch(&alias).to_string();
                let dest = layout::work_clone_dir(
                    &self.data_dir,
                    &project.agent_id,
                    &project.name,
                    &alias,
                );
                let RepoSource::Github { owner, .. } = &repo.source;
                let fetcher = Fetcher::new(self.tokens_by_owner.get(owner).cloned());
                let permit = Arc::clone(&semaphore);
                let repo = repo.clone();

                tokio::spawn(async move {
                    let _permit = permit.acquire_owned().await;
                    let result = match fetcher.pull_repo(&repo.source, &dest, &branch).await {
                        Ok(outcome) => RepoUpdateResult::Ok(outcome),
                        Err(e) => RepoUpdateResult::Err {
                            error: e.to_string(),
                        },
                    };
                    RepoUpdateReport {
                        alias,
                        branch,
                        result,
                    }
                })
            })
            .collect();

        let mut reports = Vec::with_capacity(tasks.len());
        for task in tasks {
            reports.push(task.await?);
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_testing::Lines;
    use workforce_projects::{ProjectRepo, RepoSource};

    #[test]
    fn report_serialises_ok_and_err_uniformly() {
        let ok = RepoUpdateReport {
            alias: "rails".into(),
            branch: "main".into(),
            result: RepoUpdateResult::Ok(PullOutcome::UpToDate),
        };
        let err = RepoUpdateReport {
            alias: "rails".into(),
            branch: "main".into(),
            result: RepoUpdateResult::Err {
                error: "network down".into(),
            },
        };
        let ok_json = serde_json::to_value(&ok).unwrap();
        let err_json = serde_json::to_value(&err).unwrap();
        assert_eq!(ok_json["alias"], "rails");
        assert_eq!(ok_json["result"], "up_to_date");
        assert_eq!(err_json["alias"], "rails");
        assert_eq!(err_json["error"], "network down");
    }

    const GITHUB_TOKEN: &str = "ghs_0123456789abcdefghijklmnopqrstuvwxyzAB";
    const BARE_TOKEN: &str = "q7Rk2LmZp9Vx4Nt8Bw3Hc6Jd";

    fn failed(alias: &str, error: String) -> RepoUpdateReport {
        RepoUpdateReport {
            alias: alias.into(),
            branch: "main".into(),
            result: RepoUpdateResult::Err { error },
        }
    }

    fn refresh_unreachable(token: &str) -> String {
        format!(
            "git fetch failed (exit status: 128): fatal: unable to access \
             'https://x-access-token:{token}@github.com/acme/widgets.git/': \
             Could not resolve host: github.com"
        )
    }

    fn refresh_refused(token: &str) -> String {
        format!(
            "git fetch failed (exit status: 128): remote: Repository not found.\n\
             fatal: repository 'https://x-access-token:{token}@github.com/acme/gizmos.git/' \
             not found\n"
        )
    }

    fn logged_by(run: impl FnOnce()) -> String {
        let (lines, _logging, _others) = Lines::capture();
        run();
        lines.text()
    }

    #[test]
    fn refresh_warning_names_each_failed_repo_without_its_token() {
        let reports = [
            failed("widgets", refresh_unreachable(GITHUB_TOKEN)),
            RepoUpdateReport {
                alias: "gadgets".into(),
                branch: "main".into(),
                result: RepoUpdateResult::Ok(PullOutcome::UpToDate),
            },
            failed("gizmos", refresh_refused(BARE_TOKEN)),
        ];
        let logged = logged_by(|| warn_failures("harbor", &reports));
        let warned: Vec<&str> = logged.lines().collect();
        assert_eq!(warned.len(), 2, "{logged}");
        for (line, alias) in warned.iter().zip(["widgets", "gizmos"]) {
            assert!(line.contains(" WARN "), "{line}");
            assert!(line.contains(&format!("alias={alias}")), "{line}");
        }
        assert!(
            warned[1].contains("remote: Repository not found. | fatal: repository"),
            "{logged}"
        );
        assert!(!logged.contains("gadgets"), "{logged}");
        assert!(logged.contains("project=harbor"), "{logged}");
        assert!(logged.contains("exit status: 128"), "{logged}");
        assert!(logged.contains("Could not resolve host"), "{logged}");
        assert!(!logged.contains(GITHUB_TOKEN), "{logged}");
        assert!(!logged.contains(BARE_TOKEN), "{logged}");
    }

    #[test]
    fn refresh_warning_bounds_a_long_error() {
        let unreachable = refresh_unreachable(GITHUB_TOKEN);
        let token_at = unreachable.find(GITHUB_TOKEN).unwrap();
        let padding = "x".repeat(LOGGED_ERROR_MAX_BYTES - token_at - 10);
        let error = format!("{padding}{unreachable}{}", "y".repeat(100_000));
        let logged_error = failed("widgets", error.clone()).logged_error().unwrap();
        assert!(
            logged_error.len() <= LOGGED_ERROR_MAX_BYTES + '…'.len_utf8(),
            "{}",
            logged_error.len()
        );
        assert!(logged_error.ends_with('…'));
        assert!(!logged_error.contains(&GITHUB_TOKEN[..10]));
        let logged = logged_by(|| warn_failures("harbor", &[failed("widgets", error)]));
        assert!(logged.contains("alias=widgets"), "{logged}");
        assert!(
            logged.len() < LOGGED_ERROR_MAX_BYTES * 2,
            "{}",
            logged.len()
        );
        assert!(!logged.contains(GITHUB_TOKEN), "{logged}");
    }

    #[test]
    fn refresh_warning_is_silent_when_every_repo_updated() {
        let reports = [RepoUpdateReport {
            alias: "widgets".into(),
            branch: "main".into(),
            result: RepoUpdateResult::Ok(PullOutcome::Cloned),
        }];
        assert!(reports[0].logged_error().is_none());
        assert!(logged_by(|| warn_failures("harbor", &reports)).is_empty());
    }

    #[test]
    fn updater_clamps_zero_concurrency_to_one() {
        let u = ProjectUpdater::new(HashMap::new(), PathBuf::from("/tmp"), 0);
        assert_eq!(u.concurrency, 1);
    }

    #[tokio::test]
    async fn update_returns_one_report_per_repo_in_order() {
        let u = ProjectUpdater::new(
            HashMap::new(),
            PathBuf::from("/nonexistent-test-data-dir"),
            2,
        );
        let mut p = Project::new("allan", "harbor");
        for alias in ["rails", "frontend"] {
            p.repos.push(ProjectRepo {
                alias: alias.into(),
                source: RepoSource::Github {
                    owner: "acme".into(),
                    repo: alias.into(),
                },
                default_branch: None,
                setup: None,
            });
        }
        let reports = u.update(&p).await.unwrap();
        assert_eq!(reports.len(), 2);
        // Both should fail (path doesn't exist + no network), proving the
        // updater surfaces per-repo errors instead of aborting on first.
        assert!(matches!(reports[0].result, RepoUpdateResult::Err { .. }));
        assert!(matches!(reports[1].result, RepoUpdateResult::Err { .. }));
    }
}
