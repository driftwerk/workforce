//! Multi-repo project definitions.
//!
//! A project groups a set of related repositories that share a single
//! code graph so impact analysis and search can cross repo boundaries.
//! Projects are declared as TOML files loaded at startup; this crate only
//! parses them — discovery (where the files live on disk) is the host's
//! concern.
//!
//! ## Two repo modes
//!
//! - **Legacy:** `path = "/abs/path/to/clone"` points at an existing local
//!   working tree. No auto-pull, no auto-clone — the caller manages it.
//! - **Mirror:** `source = { kind = "github", owner, repo }` plus a project
//!   `mirror_root` (or the default `<data_dir>/mirrors/<project>/`). The
//!   server clones the repo into `<mirror_root>/<repo.name>` and the
//!   scheduler keeps it pristine via `git fetch` + `git reset --hard`.
//!
//! Each repo must set exactly one of `path` or `source`; this is enforced
//! at load time so runtime never has to handle the "neither set" case.

use std::path::{Path, PathBuf};

use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::schedule::ScheduleSpec;

/// A set of related repositories indexed into a single graph.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    /// Where mirror clones live for this project. When omitted, defaults to
    /// `<data_dir>/mirrors/<project_name>/`. Only relevant for repos that
    /// declare a `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_root: Option<PathBuf>,
    /// Project-wide auto-pull / re-index schedule. Repos inherit this and
    /// can override individual fields in their own `[repos.schedule]` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

/// One repo within a project. Either `path` (legacy) or `source` (mirror)
/// must be set; load-time validation enforces this.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    /// Explicit local path to an existing working tree. Mutually exclusive
    /// with `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Upstream coordinates for cloning into the project mirror root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RepoSource>,
    /// Branch the mirror tracks. Defaults to `"main"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Per-repo schedule override. Field-merged onto the project schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
}

/// Upstream coordinates for an auto-cloned repo. Tagged so we can grow into
/// `git`/`ssh`/`path` without a breaking schema change.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum RepoSource {
    /// Cloned from `git@github.com:<owner>/<repo>.git` by default.
    Github { owner: String, repo: String },
    /// Direct git URL — escape hatch for non-GitHub or special remotes.
    Git { url: String },
}

impl RepoSource {
    /// Default clone URL. GitHub uses SSH so the user's existing
    /// `git@github.com` config and SSH agent just work.
    pub fn clone_url(&self) -> String {
        match self {
            RepoSource::Github { owner, repo } => format!("git@github.com:{owner}/{repo}.git"),
            RepoSource::Git { url } => url.clone(),
        }
    }
}

impl ProjectConfig {
    /// Parse a project config from a TOML file and validate it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading project config {}", path.display()))?;
        let cfg: ProjectConfig = toml::from_str(&text)
            .wrap_err_with(|| format!("parsing project config {}", path.display()))?;
        cfg.validate()
            .wrap_err_with(|| format!("validating project config {}", path.display()))?;
        Ok(cfg)
    }

    /// Discover and load every `*.toml` file directly under `dir`. Returns an
    /// empty Vec if the directory does not exist (no projects configured is a
    /// valid state).
    pub fn load_all_from_dir(dir: &Path) -> Result<Vec<Self>> {
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .wrap_err_with(|| format!("reading projects dir {}", dir.display()))?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort();
        entries.iter().map(|p| Self::load(p)).collect()
    }

    /// Load-time validation — enforces "exactly one of path/source" per repo
    /// and rejects schedule blocks that fail to parse so we never have to
    /// handle malformed schedules at tick time.
    fn validate(&self) -> Result<()> {
        for repo in &self.repos {
            match (repo.path.as_ref(), repo.source.as_ref()) {
                (Some(_), Some(_)) => {
                    return Err(eyre!(
                        "repo \"{}\" sets both `path` and `source`; pick one",
                        repo.name
                    ));
                }
                (None, None) => {
                    return Err(eyre!(
                        "repo \"{}\" must set either `path` (legacy) or `source` (mirror)",
                        repo.name
                    ));
                }
                _ => {}
            }
        }
        // Probe the schedule: if it's unparseable we want to know now, not
        // when the scheduler tries to fire.
        let project_schedule = self.schedule.clone().unwrap_or_default();
        if self.schedule.is_some() {
            project_schedule
                .clone()
                .into_effective()
                .wrap_err("project schedule")?;
        }
        for repo in &self.repos {
            if let Some(repo_sched) = &repo.schedule {
                let merged = repo_sched.merge(&project_schedule);
                merged
                    .into_effective()
                    .wrap_err_with(|| format!("schedule for repo \"{}\"", repo.name))?;
            }
        }
        Ok(())
    }

    /// Resolve where mirror clones for this project should live. Explicit
    /// `mirror_root` wins; otherwise `<data_dir>/mirrors/<project_name>`.
    pub fn effective_mirror_root(&self, data_dir: &Path) -> PathBuf {
        if let Some(p) = &self.mirror_root {
            return p.clone();
        }
        data_dir.join("mirrors").join(&self.name)
    }
}

impl RepoConfig {
    /// Resolve the repo's working path. Legacy `path` is returned verbatim;
    /// mirror repos resolve to `<effective_mirror_root>/<name>`.
    pub fn effective_path(&self, project: &ProjectConfig, data_dir: &Path) -> PathBuf {
        if let Some(p) = &self.path {
            return p.clone();
        }
        project.effective_mirror_root(data_dir).join(&self.name)
    }

    /// Returns `true` for mirror-mode repos (those with `source`).
    pub fn is_mirror(&self) -> bool {
        self.source.is_some()
    }

    /// Tracked branch for mirror repos; defaults to `"main"`.
    pub fn effective_default_branch(&self) -> &str {
        self.default_branch.as_deref().unwrap_or("main")
    }

    /// Effective schedule after merging project + repo blocks. Returns
    /// `Ok(None)` when the repo has no schedule, the schedule is disabled,
    /// or the repo is in legacy `path` mode (mirror-only feature).
    pub fn effective_schedule(
        &self,
        project: &ProjectConfig,
    ) -> Result<Option<crate::schedule::EffectiveSchedule>> {
        if !self.is_mirror() {
            return Ok(None);
        }
        let project_schedule = project.schedule.clone();
        let repo_schedule = self.schedule.clone();
        let merged = match (repo_schedule, project_schedule) {
            (Some(r), Some(p)) => r.merge(&p),
            (Some(r), None) => r,
            (None, Some(p)) => p,
            (None, None) => return Ok(None),
        };
        merged.into_effective()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_parses_legacy_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("harbor.toml");
        std::fs::write(
            &file,
            r#"
name = "harbor"

[[repos]]
name = "backend"
path = "/repos/HarborRails"

[[repos]]
name = "web"
path = "/repos/HarborApps"
"#,
        )
        .unwrap();

        let cfg = ProjectConfig::load(&file).unwrap();
        assert_eq!(cfg.name, "harbor");
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].name, "backend");
        assert_eq!(
            cfg.repos[1].path.as_deref(),
            Some(Path::new("/repos/HarborApps"))
        );
        assert!(!cfg.repos[0].is_mirror());
    }

    #[test]
    fn load_parses_mirror_toml_with_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("harbor.toml");
        std::fs::write(
            &file,
            r#"
name = "harbor"

[schedule]
every   = "15m"
reindex = "incremental"

[[repos]]
name = "rails"
source = { kind = "github", owner = "harborlabs", repo = "HarborRails" }
"#,
        )
        .unwrap();

        let cfg = ProjectConfig::load(&file).unwrap();
        assert!(cfg.schedule.is_some());
        assert!(cfg.repos[0].is_mirror());
        assert_eq!(
            cfg.repos[0].source,
            Some(RepoSource::Github {
                owner: "harborlabs".into(),
                repo: "HarborRails".into()
            })
        );
    }

    #[test]
    fn rejects_repo_with_both_path_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bad.toml");
        std::fs::write(
            &file,
            r#"
name = "bad"
[[repos]]
name = "x"
path = "/x"
source = { kind = "github", owner = "a", repo = "b" }
"#,
        )
        .unwrap();
        let err = ProjectConfig::load(&file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("both `path` and `source`"), "got: {msg}");
    }

    #[test]
    fn rejects_repo_with_neither_path_nor_source() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bad.toml");
        std::fs::write(
            &file,
            r#"
name = "bad"
[[repos]]
name = "x"
"#,
        )
        .unwrap();
        let err = ProjectConfig::load(&file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("must set either"), "got: {msg}");
    }

    #[test]
    fn rejects_unparseable_schedule_at_load() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bad.toml");
        std::fs::write(
            &file,
            r#"
name = "bad"
[schedule]
every = "fast"

[[repos]]
name = "x"
source = { kind = "github", owner = "a", repo = "b" }
"#,
        )
        .unwrap();
        let err = ProjectConfig::load(&file).unwrap_err();
        // The chained context lands somewhere in the message.
        let msg = format!("{err:#}");
        assert!(msg.contains("schedule.every"), "got: {msg}");
    }

    #[test]
    fn load_all_returns_empty_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let projects = ProjectConfig::load_all_from_dir(&missing).unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn load_all_reads_sorted_toml_files() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("b.toml", "name = \"b\"\n"),
            ("a.toml", "name = \"a\"\n"),
            ("ignored.json", "{}"),
        ] {
            std::fs::write(tmp.path().join(name), body).unwrap();
        }

        let projects = ProjectConfig::load_all_from_dir(tmp.path()).unwrap();
        let names: Vec<_> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn load_missing_file_is_an_error() {
        let err = ProjectConfig::load(Path::new("/does/not/exist.toml")).unwrap_err();
        assert!(err.to_string().contains("reading project config"));
    }

    #[test]
    fn effective_mirror_root_explicit_wins() {
        let cfg = ProjectConfig {
            name: "p".into(),
            mirror_root: Some(PathBuf::from("/custom/mirrors/p")),
            schedule: None,
            repos: Vec::new(),
        };
        assert_eq!(
            cfg.effective_mirror_root(Path::new("/data")),
            PathBuf::from("/custom/mirrors/p")
        );
    }

    #[test]
    fn effective_mirror_root_default_under_data_dir() {
        let cfg = ProjectConfig {
            name: "p".into(),
            mirror_root: None,
            schedule: None,
            repos: Vec::new(),
        };
        assert_eq!(
            cfg.effective_mirror_root(Path::new("/data")),
            PathBuf::from("/data/mirrors/p")
        );
    }

    #[test]
    fn effective_path_legacy_returns_explicit_path() {
        let project = ProjectConfig {
            name: "p".into(),
            mirror_root: None,
            schedule: None,
            repos: Vec::new(),
        };
        let repo = RepoConfig {
            name: "x".into(),
            path: Some(PathBuf::from("/abs/x")),
            source: None,
            default_branch: None,
            schedule: None,
        };
        assert_eq!(
            repo.effective_path(&project, Path::new("/data")),
            PathBuf::from("/abs/x")
        );
    }

    #[test]
    fn effective_path_mirror_uses_mirror_root_and_name() {
        let project = ProjectConfig {
            name: "harbor".into(),
            mirror_root: None,
            schedule: None,
            repos: Vec::new(),
        };
        let repo = RepoConfig {
            name: "rails".into(),
            path: None,
            source: Some(RepoSource::Github {
                owner: "harborlabs".into(),
                repo: "HarborRails".into(),
            }),
            default_branch: None,
            schedule: None,
        };
        assert_eq!(
            repo.effective_path(&project, Path::new("/data")),
            PathBuf::from("/data/mirrors/harbor/rails")
        );
    }

    #[test]
    fn effective_schedule_returns_none_for_legacy_repos() {
        let project = ProjectConfig {
            name: "p".into(),
            mirror_root: None,
            schedule: Some(ScheduleSpec {
                every: Some("15m".into()),
                ..ScheduleSpec::default()
            }),
            repos: Vec::new(),
        };
        let repo = RepoConfig {
            name: "x".into(),
            path: Some(PathBuf::from("/abs/x")),
            source: None,
            default_branch: None,
            schedule: None,
        };
        assert!(repo.effective_schedule(&project).unwrap().is_none());
    }

    #[test]
    fn effective_schedule_inherits_project_for_mirror_repo() {
        let project = ProjectConfig {
            name: "p".into(),
            mirror_root: None,
            schedule: Some(ScheduleSpec {
                every: Some("15m".into()),
                reindex: Some("incremental".into()),
                ..ScheduleSpec::default()
            }),
            repos: Vec::new(),
        };
        let repo = RepoConfig {
            name: "x".into(),
            path: None,
            source: Some(RepoSource::Github {
                owner: "a".into(),
                repo: "b".into(),
            }),
            default_branch: None,
            schedule: None,
        };
        let eff = repo.effective_schedule(&project).unwrap().unwrap();
        assert_eq!(eff.every, std::time::Duration::from_secs(900));
    }

    #[test]
    fn effective_schedule_repo_disable_wins() {
        let project = ProjectConfig {
            name: "p".into(),
            mirror_root: None,
            schedule: Some(ScheduleSpec {
                every: Some("15m".into()),
                ..ScheduleSpec::default()
            }),
            repos: Vec::new(),
        };
        let repo = RepoConfig {
            name: "x".into(),
            path: None,
            source: Some(RepoSource::Github {
                owner: "a".into(),
                repo: "b".into(),
            }),
            default_branch: None,
            schedule: Some(ScheduleSpec {
                enabled: Some(false),
                ..ScheduleSpec::default()
            }),
        };
        assert!(repo.effective_schedule(&project).unwrap().is_none());
    }

    #[test]
    fn repo_source_github_default_url_is_ssh() {
        let s = RepoSource::Github {
            owner: "harborlabs".into(),
            repo: "HarborRails".into(),
        };
        assert_eq!(s.clone_url(), "git@github.com:harborlabs/HarborRails.git");
    }

    #[test]
    fn repo_source_git_passes_url_through() {
        let s = RepoSource::Git {
            url: "ssh://git@gitea.example.com/x/y.git".into(),
        };
        assert_eq!(s.clone_url(), "ssh://git@gitea.example.com/x/y.git");
    }

    /// Backwards-compat regression: every TOML the user has on disk today
    /// must continue to parse and resolve. These are copied verbatim from
    /// `~/Library/Application Support/workforce-local/projects/` as of the
    /// commit that introduces this schema change.
    #[test]
    fn legacy_user_tomls_still_parse() {
        let cases: &[(&str, &str)] = &[
            (
                "harbor",
                r#"
name = "harbor"

[[repos]]
name = "rails"
path = "/home/dev/projects/harborlabs/HarborRails"

[[repos]]
name = "frontend"
path = "/home/dev/projects/harborlabs/HarborApps"

[[repos]]
name = "core-api"
path = "/home/dev/projects/harborlabs/core-api"
"#,
            ),
            (
                "driftwerk",
                r#"
name = "driftwerk"

[[repos]]
name = "orbit"
path = "/home/dev/projects/driftwerk/orbit"

[[repos]]
name = "workforce"
path = "/home/dev/projects/driftwerk/workforce"
"#,
            ),
            (
                "fabrikam",
                r#"
name = "fabrikam"

[[repos]]
name = "backend"
path = "/home/dev/projects/fabrikam/backend"

[[repos]]
name = "frontend"
path = "/home/dev/projects/fabrikam/frontend"
"#,
            ),
            (
                "contoso",
                r#"
name = "contoso"

[[repos]]
name = "backend"
path = "/home/dev/projects/contoso/monorepo"

[[repos]]
name = "keycloak"
path = "/home/dev/projects/contoso/contoso-keycloak"
"#,
            ),
        ];

        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in cases {
            let file = tmp.path().join(format!("{name}.toml"));
            std::fs::write(&file, body).unwrap();
            let cfg = ProjectConfig::load(&file)
                .unwrap_or_else(|e| panic!("legacy TOML \"{name}\" must still parse: {e:#}"));
            assert_eq!(cfg.name, *name);
            for repo in &cfg.repos {
                assert!(
                    repo.path.is_some(),
                    "legacy repo \"{}\" should keep its explicit path",
                    repo.name
                );
                assert!(!repo.is_mirror());
                assert_eq!(
                    repo.effective_path(&cfg, Path::new("/data")),
                    repo.path.clone().unwrap()
                );
            }
        }
    }
}
