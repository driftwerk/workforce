//! Bounded-parallel pull across every repo in a project.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use serde::Serialize;
use tokio::sync::Semaphore;

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
