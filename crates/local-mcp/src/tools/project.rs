//! `project.*` MCP tools — manage multi-repo projects.
//!
//! A project is a set of related repos declared in a single TOML file under
//! the server's `projects_dir`. Indexing a project writes every repo into the
//! shared code graph, which lets `impact.cross_repo` and future tools walk
//! edges that span repos (e.g. HTTP route ↔ client call in Phase 2.5's next
//! cut). For mirror-mode projects, `project.sync` materialises the mirror
//! clones and the background scheduler keeps them current — see
//! [`crate::mirror`].

use std::path::Path;

use chrono::{DateTime, Utc};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use workforce_knowledge_service::{ProjectConfig, ProjectIndexReport, RepoIndexReport};

use crate::mirror::RepoMirrorState;
use crate::LocalMcpServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexProjectArgs {
    /// Project name (must match a TOML file under the server's projects_dir).
    pub name: String,
    /// If true, only reprocess files whose content has changed since the
    /// previous index. Use this for a quick refresh; falls back to a full
    /// index per repo when no prior index exists. Default: false (full).
    #[serde(default)]
    pub incremental: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncProjectArgs {
    /// Project name (must match a TOML file under the server's projects_dir).
    pub name: String,
}

#[derive(Debug, Serialize)]
struct RepoSummary {
    name: String,
    path: String,
    repo_id: String,
    /// `true` when the repo is auto-pulled from `source` into a mirror.
    is_mirror: bool,
    /// For mirror repos: timestamp of last successful pull, or `null` when
    /// the scheduler hasn't fired yet and `project.sync` hasn't run. Always
    /// `null` for legacy `path` repos (the scheduler doesn't manage them).
    /// Always emitted so "never pulled" is distinguishable from "field
    /// missing."
    last_pulled_at: Option<DateTime<Utc>>,
    /// Timestamp of last successful re-index after a pull. `null` when no
    /// indexed pull has completed yet, or when the schedule has
    /// `reindex = "off"`.
    last_indexed_at: Option<DateTime<Utc>>,
    /// Last sync error verbatim, or `null` when the most recent sync
    /// succeeded (or no sync has run).
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProjectSummary {
    name: String,
    repos: Vec<RepoSummary>,
}

fn project_summary(
    project: &ProjectConfig,
    data_dir: &Path,
    mirror_state: impl Fn(&str, &str) -> Option<RepoMirrorState>,
) -> ProjectSummary {
    ProjectSummary {
        name: project.name.clone(),
        repos: project
            .repos
            .iter()
            .map(|r| {
                let path = r.effective_path(project, data_dir);
                let state = if r.is_mirror() {
                    mirror_state(&project.name, &r.name)
                } else {
                    None
                };
                RepoSummary {
                    name: r.name.clone(),
                    path: path.display().to_string(),
                    repo_id: workforce_knowledge_service::KnowledgeService::repo_id_for(&path),
                    is_mirror: r.is_mirror(),
                    last_pulled_at: state.as_ref().and_then(|s| s.last_pulled_at),
                    last_indexed_at: state.as_ref().and_then(|s| s.last_indexed_at),
                    last_error: state.as_ref().and_then(|s| s.last_error.clone()),
                }
            })
            .collect(),
    }
}

#[derive(Debug, Serialize)]
struct RepoIndexSummary {
    name: String,
    path: String,
    repo_id: String,
    #[serde(flatten)]
    outcome: IndexOutcome,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum IndexOutcome {
    Success {
        files_indexed: usize,
        entities_created: usize,
        relationships_created: usize,
        duration_ms: u64,
    },
    Failure {
        error: String,
    },
}

impl From<&RepoIndexReport> for RepoIndexSummary {
    fn from(r: &RepoIndexReport) -> Self {
        let outcome = match &r.outcome {
            Ok(res) => IndexOutcome::Success {
                files_indexed: res.files_indexed,
                entities_created: res.entities_created,
                relationships_created: res.relationships_created,
                duration_ms: res.duration_ms,
            },
            Err(e) => IndexOutcome::Failure {
                error: e.to_string(),
            },
        };
        Self {
            name: r.name.clone(),
            path: r.path.display().to_string(),
            repo_id: r.repo_id.clone(),
            outcome,
        }
    }
}

fn project_index_summary(report: &ProjectIndexReport) -> serde_json::Value {
    serde_json::json!({
        "project": report.project_name,
        "repos": report.repos.iter().map(RepoIndexSummary::from).collect::<Vec<_>>(),
    })
}

#[tool_router(router = project_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "project.list",
        description = "List projects defined under the server's projects_dir. Each project \
                       bundles a set of related repos; `project.index` indexes all of them \
                       into a shared code graph so `impact.cross_repo` can walk across them. \
                       Every repo entry includes `is_mirror`, `last_pulled_at`, \
                       `last_indexed_at`, and `last_error`. For mirror-mode repos these \
                       populate after a tick or `project.sync`; before that, all three are \
                       `null`. For legacy `path` repos they are always `null` (the scheduler \
                       does not manage them)."
    )]
    async fn project_list(&self) -> Result<String, String> {
        let projects = ProjectConfig::load_all_from_dir(&self.state.projects_dir)
            .map_err(|e| format!("loading projects: {e}"))?;

        let mirror = self.state.mirror.clone();
        let data_dir = self.state.data_dir.clone();
        let summaries: Vec<ProjectSummary> = projects
            .iter()
            .map(|p| project_summary(p, &data_dir, |proj, repo| mirror.state_for(proj, repo)))
            .collect();
        Ok(serde_json::json!({
            "projects_dir": self.state.projects_dir.display().to_string(),
            "projects": summaries,
        })
        .to_string())
    }

    #[tool(
        name = "project.index",
        description = "Index ALL repos in a named multi-repo project into the shared code \
                       graph. Use this when the user refers to a project by name (e.g. \
                       \"index the harbor project\"). For indexing a single repo, use \
                       `codegraph.index_repo` instead. Pass `incremental=true` for a quick \
                       refresh that only reprocesses files changed since the last index — \
                       typically ~1s per repo vs ~30s for a full reindex. Returns a per-repo \
                       report; partial failures are captured in-line rather than aborting."
    )]
    async fn project_index(
        &self,
        Parameters(args): Parameters<IndexProjectArgs>,
    ) -> Result<String, String> {
        let project = find_project(&self.state.projects_dir, &args.name)?;
        let report = self
            .state
            .knowledge
            .index_project(&project, &self.state.data_dir, args.incremental)
            .await;
        Ok(project_index_summary(&report).to_string())
    }

    #[tool(
        name = "project.sync",
        description = "Force-clone any missing mirror repos for the named project and pull \
                       every existing one to upstream HEAD, then incrementally re-index. Use \
                       this after editing a project TOML or when you want a fresh state \
                       without waiting for the next scheduled tick. Legacy repos that point \
                       at an explicit `path` are skipped (the scheduler can't manage them). \
                       Returns one entry per mirror repo with success or error."
    )]
    async fn project_sync(
        &self,
        Parameters(args): Parameters<SyncProjectArgs>,
    ) -> Result<String, String> {
        let project = find_project(&self.state.projects_dir, &args.name)?;
        let results = self.state.mirror.force_sync_project(&project).await;
        let repos: Vec<serde_json::Value> = results
            .iter()
            .map(|(name, outcome)| match outcome {
                Ok(()) => serde_json::json!({ "name": name, "ok": true }),
                Err(e) => {
                    serde_json::json!({ "name": name, "ok": false, "error": format!("{e:#}") })
                }
            })
            .collect();
        Ok(serde_json::json!({
            "project": project.name,
            "synced": repos,
        })
        .to_string())
    }
}

pub(crate) fn find_project(
    projects_dir: &std::path::Path,
    name: &str,
) -> Result<ProjectConfig, String> {
    let projects = ProjectConfig::load_all_from_dir(projects_dir)
        .map_err(|e| format!("loading projects: {e}"))?;
    projects
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("no project named '{name}' under {}", projects_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use workforce_knowledge_service::RepoConfig;

    fn legacy_project() -> ProjectConfig {
        ProjectConfig {
            name: "harbor".into(),
            mirror_root: None,
            schedule: None,
            repos: vec![RepoConfig {
                name: "backend".into(),
                path: Some(PathBuf::from("/repos/backend")),
                source: None,
                default_branch: None,
                schedule: None,
            }],
        }
    }

    #[test]
    fn project_summary_derives_repo_id_and_path() {
        let summary = project_summary(&legacy_project(), Path::new("/data"), |_, _| None);
        assert_eq!(summary.name, "harbor");
        assert_eq!(summary.repos.len(), 1);
        assert_eq!(summary.repos[0].name, "backend");
        assert_eq!(summary.repos[0].path, "/repos/backend");
        assert!(!summary.repos[0].is_mirror);
        assert!(summary.repos[0].last_pulled_at.is_none());
        assert!(!summary.repos[0].repo_id.is_empty());
    }

    #[test]
    fn project_summary_marks_mirror_repos_and_uses_mirror_state() {
        let project = ProjectConfig {
            name: "demo".into(),
            mirror_root: Some(PathBuf::from("/mirrors/demo")),
            schedule: None,
            repos: vec![RepoConfig {
                name: "rails".into(),
                path: None,
                source: Some(workforce_knowledge_service::RepoSource::Github {
                    owner: "a".into(),
                    repo: "b".into(),
                }),
                default_branch: None,
                schedule: None,
            }],
        };
        let now = Utc::now();
        let summary = project_summary(&project, Path::new("/data"), |proj, repo| {
            assert_eq!(proj, "demo");
            assert_eq!(repo, "rails");
            Some(RepoMirrorState {
                last_pulled_at: Some(now),
                last_indexed_at: Some(now),
                last_error: None,
            })
        });
        assert!(summary.repos[0].is_mirror);
        assert_eq!(summary.repos[0].path, "/mirrors/demo/rails");
        assert_eq!(summary.repos[0].last_pulled_at, Some(now));
        assert_eq!(summary.repos[0].last_indexed_at, Some(now));
        assert!(summary.repos[0].last_error.is_none());
    }

    #[test]
    fn find_project_errors_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_project(tmp.path(), "nope").unwrap_err();
        assert!(err.contains("no project named 'nope'"));
    }

    #[test]
    fn find_project_returns_matching_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("harbor.toml"),
            "name = \"harbor\"\n[[repos]]\nname = \"b\"\npath = \"/b\"\n",
        )
        .unwrap();

        let found = find_project(tmp.path(), "harbor").unwrap();
        assert_eq!(found.name, "harbor");
        assert_eq!(found.repos.len(), 1);
    }
}
