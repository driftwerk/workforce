//! Per-repo workspace setup — the dependency install or codegen a fresh
//! worktree needs before Claude Code can verify anything — run once per scope
//! after materialisation, so a round starts with a ready tree instead of
//! spending its first turns discovering and installing the toolchain.

use std::path::Path;
use std::time::Duration;

use tracing::warn;
use workforce_agent::tools::exec::EXEC_SECRET_VARS;
use workforce_agent::tools::exec_runner::{
    run_or_dispatch, run_setup, EnvMode, ExecRequest, RunnerEndpoint,
};
pub use workforce_agent::tools::exec_runner::{SetupOutcome, SetupReport};
use workforce_projects::Project;

/// Written into the session root once every setup command has actually run,
/// success or failure, so a broken setup is reported once rather than re-run
/// every round. A command that could not be started at all — the runner was
/// unreachable — leaves no marker, so the next round tries again.
pub const SETUP_MARKER: &str = ".workforce-setup";

/// Where and how setup commands run.
pub struct SetupRunner {
    /// The privilege-separated runner when configured; `None` runs in-process.
    pub runner: Option<RunnerEndpoint>,
    /// Environment the commands share with the round, so what setup installs is
    /// what the round finds.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// Run each repo's setup command in its worktree under `session_root`, once
/// per scope. Returns the reports for the pass that ran, and nothing when the
/// scope was already set up or the project configures no setup. Runs the
/// commands the way `exec.run` does on this install: through the runner when
/// one is configured, else in-process without the hub's own secrets.
pub async fn run_once(
    project: &Project,
    session_root: &Path,
    runner: &SetupRunner,
) -> Vec<SetupReport> {
    let commands: Vec<(&str, &str)> = project
        .repos
        .iter()
        .filter_map(|repo| {
            project
                .effective_setup(&repo.alias)
                .map(|command| (repo.alias.as_str(), command))
        })
        .collect();
    let marker = session_root.join(SETUP_MARKER);
    if commands.is_empty() || marker.exists() {
        return Vec::new();
    }
    let commands: Vec<(String, String)> = commands
        .into_iter()
        .map(|(alias, command)| (alias.to_string(), command.to_string()))
        .collect();
    let reports = run_setup(&commands, |alias, command| {
        let req = ExecRequest {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command],
            workdir: Some(session_root.join(alias).display().to_string()),
            timeout_secs: runner.timeout.as_secs(),
            allow_retry: false,
            env: runner.env.clone(),
            stdin: None,
            scope: None,
            mcp: None,
        };
        run_or_dispatch(
            runner.runner.as_ref(),
            req,
            &[],
            EnvMode::Inherit {
                remove: EXEC_SECRET_VARS,
            },
        )
    })
    .await;
    for report in reports.iter().filter(|r| r.outcome != SetupOutcome::Ok) {
        warn!(
            alias = report.alias,
            command = report.command,
            outcome = ?report.outcome,
            "workspace setup did not succeed"
        );
    }
    if reports.iter().all(|r| r.outcome.ran()) {
        if let Err(e) = tokio::fs::write(&marker, b"").await {
            warn!(path = %marker.display(), error = %e, "workspace setup: failed to write marker");
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use workforce_projects::{ProjectRepo, RepoSource};

    fn repo(alias: &str, setup: Option<&str>) -> ProjectRepo {
        ProjectRepo {
            alias: alias.into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: alias.into(),
            },
            default_branch: None,
            setup: setup.map(str::to_string),
        }
    }

    fn project(setup: Option<&str>) -> Project {
        let mut project = Project::new("allan", "harbor");
        project.defaults.setup = setup.map(str::to_string);
        project.repos.push(repo("rails", None));
        project
    }

    fn runner(timeout: Duration) -> SetupRunner {
        SetupRunner {
            runner: None,
            env: vec![("WF_SETUP_PROBE".into(), "warm".into())],
            timeout,
        }
    }

    async fn session_with_repo(tmp: &TempDir) -> PathBuf {
        let root = tmp.path().join("session");
        tokio::fs::create_dir_all(root.join("rails")).await.unwrap();
        root
    }

    #[tokio::test]
    async fn runs_each_repo_setup_once_in_its_worktree_with_the_round_env() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        let project = project(Some("echo \"$WF_SETUP_PROBE\" > ran.txt"));

        let reports = run_once(&project, &root, &runner(Duration::from_secs(30))).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].alias, "rails");
        assert_eq!(reports[0].outcome, SetupOutcome::Ok);
        assert_eq!(
            std::fs::read_to_string(root.join("rails").join("ran.txt"))
                .unwrap()
                .trim(),
            "warm"
        );
        assert!(root.join(SETUP_MARKER).exists());

        let again = run_once(&project, &root, &runner(Duration::from_secs(30))).await;
        assert!(again.is_empty(), "a set-up scope is not set up again");
    }

    #[tokio::test]
    async fn a_failing_setup_is_reported_once_and_not_retried() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        let project = project(Some("echo boom >&2; exit 3"));

        let reports = run_once(&project, &root, &runner(Duration::from_secs(30))).await;
        assert_eq!(
            reports[0].outcome,
            SetupOutcome::Failed {
                exit_code: 3,
                stderr_tail: "boom".into()
            }
        );
        assert_eq!(
            reports[0].describe(),
            "`rails`: `echo boom >&2; exit 3` — failed (exit 3): boom"
        );
        assert!(root.join(SETUP_MARKER).exists());
        assert!(run_once(&project, &root, &runner(Duration::from_secs(30)))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_setup_past_its_deadline_is_timed_out() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        let project = project(Some("sleep 30"));

        let started = std::time::Instant::now();
        let reports = run_once(&project, &root, &runner(Duration::from_secs(1))).await;
        assert_eq!(reports[0].outcome, SetupOutcome::TimedOut);
        assert_eq!(reports[0].describe(), "`rails`: `sleep 30` — timed out");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn a_repo_override_runs_in_place_of_the_project_default() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        tokio::fs::create_dir_all(root.join("web")).await.unwrap();
        let mut project = project(Some("echo default > which"));
        project
            .repos
            .push(repo("web", Some("echo override > which")));

        let reports = run_once(&project, &root, &runner(Duration::from_secs(30))).await;
        assert_eq!(
            reports
                .iter()
                .map(|r| r.command.as_str())
                .collect::<Vec<_>>(),
            vec!["echo default > which", "echo override > which"]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("rails/which"))
                .unwrap()
                .trim(),
            "default"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("web/which"))
                .unwrap()
                .trim(),
            "override"
        );
    }

    /// A runner that cannot be reached is not a setup that failed: nothing
    /// ran, so nothing is marked and the next round tries again.
    #[tokio::test]
    async fn an_unreachable_runner_leaves_no_marker() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        let project = project(Some("touch ran"));
        let runner = SetupRunner {
            runner: Some(RunnerEndpoint::Unix(tmp.path().join("absent.sock"))),
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        };

        let reports = run_once(&project, &root, &runner).await;
        assert!(matches!(
            reports[0].outcome,
            SetupOutcome::Unavailable { .. }
        ));
        assert!(reports[0].describe().contains("could not run"));
        assert!(
            !root.join("rails/ran").exists(),
            "fail-closed: nothing ran locally"
        );
        assert!(!root.join(SETUP_MARKER).exists());
    }

    #[tokio::test]
    async fn a_project_without_setup_runs_nothing_and_leaves_no_marker() {
        let tmp = TempDir::new().unwrap();
        let root = session_with_repo(&tmp).await;
        let reports = run_once(&project(None), &root, &runner(Duration::from_secs(30))).await;
        assert!(reports.is_empty());
        assert!(!root.join(SETUP_MARKER).exists());
    }

    #[test]
    fn a_report_serialises_flat_for_the_tool_result() {
        let report = SetupReport {
            alias: "rails".into(),
            command: "bundle install".into(),
            outcome: SetupOutcome::Ok,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({ "alias": "rails", "command": "bundle install", "status": "ok" })
        );
    }
}
