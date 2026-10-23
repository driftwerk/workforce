//! Publishing an installed skill to a catalog: a pull request against the
//! catalog's repository holding the skill's files and its `catalog.json` entry
//!. The flow is one for both kinds; a
//! [`Publication`] supplies what differs — the files, the manifest the
//! repository keeps, and the lines of the pull request.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{bail, eyre, Report, Result, WrapErr};
use serde::Deserialize;
use tracing::info;
use workforce_common::settings::CatalogSource;
use workforce_skills::adapters::github::client::GithubClient;
use workforce_skills::catalog::{self, CatalogEntry, CatalogManifest, SkillKind};

use crate::git_hardening::{hardened_git, lock_down_config};
use crate::skill_access::kind_word;
use crate::skill_catalog::select_catalog;
use crate::wasm_skill_tools::is_commit_sha;

const GITHUB_API: &str = "https://api.github.com";

// ─── the remote ─────────────────────────────────────────────────────────────

pub struct PullRequestSpec {
    pub title: String,
    pub head: String,
    pub base: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub url: String,
    pub number: u64,
}

/// Where a publish pushes to and opens its pull request. Production is
/// GitHub; tests use a bare local repository.
#[async_trait]
pub trait PublishRemote: Send + Sync {
    fn clone_url(&self) -> String;
    /// Remove any credential the clone URL carries from text about to be
    /// surfaced (git stderr echoes the URL).
    fn scrub(&self, text: &str) -> String;
    async fn open_pull_request(&self, spec: PullRequestSpec) -> Result<PullRequest>;
}

pub struct GithubRemote {
    owner_repo: String,
    token: String,
}

#[async_trait]
impl PublishRemote for GithubRemote {
    fn clone_url(&self) -> String {
        format!(
            "https://x-access-token:{}@github.com/{}.git",
            self.token, self.owner_repo
        )
    }

    fn scrub(&self, text: &str) -> String {
        text.replace(&self.token, "***")
    }

    async fn open_pull_request(&self, spec: PullRequestSpec) -> Result<PullRequest> {
        let created = GithubClient::new(&self.token, GITHUB_API)?
            .post(
                &format!("/repos/{}/pulls", self.owner_repo),
                &serde_json::json!({
                    "title": spec.title,
                    "head": spec.head,
                    "base": spec.base,
                    "body": spec.body,
                }),
            )
            .await?;
        let url = created["html_url"]
            .as_str()
            .ok_or_else(|| eyre!("GitHub's pull request response carries no html_url"))?;
        let number = created["number"]
            .as_u64()
            .ok_or_else(|| eyre!("GitHub's pull request response carries no number"))?;
        Ok(PullRequest {
            url: url.to_string(),
            number,
        })
    }
}

/// The caller's GitHub token for a repository owner.
pub type TokenFor<'a> = dyn Fn(&str) -> Option<String> + Send + Sync + 'a;

/// Builds the remote for a catalog; the production factory is where the
/// publishing credential is resolved.
pub type RemoteFactory = Arc<
    dyn Fn(&CatalogSource, &TokenFor<'_>) -> Result<Arc<dyn PublishRemote>, String> + Send + Sync,
>;

/// Publishing writes to an org repository and must be attributable, so it
/// uses the caller's own GitHub credential for the repo's owner — never the
/// catalog read token, which the settings keep deliberately separate.
pub fn github_remote(
    source: &CatalogSource,
    token_for: &TokenFor<'_>,
) -> Result<Arc<dyn PublishRemote>, String> {
    let owner_repo = catalog::github_owner_repo(&source.repo).map_err(|e| e.to_string())?;
    let owner = owner_repo.split('/').next().unwrap_or_default().to_string();
    let token = token_for(&owner).ok_or_else(|| {
        format!(
            "No GitHub token to publish to {owner_repo}. Store one at \
             skills/github/workspaces/{owner}/token (or skills/github/token): publishing \
             writes to the catalog repo with your own credential, not the catalog's read token."
        )
    })?;
    Ok(Arc::new(GithubRemote { owner_repo, token }))
}

// ─── the plan ───────────────────────────────────────────────────────────────

/// The installed skill a publish ships: what its catalog entry says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub kind: SkillKind,
    pub name: String,
    pub version: String,
    pub description: String,
    /// The binaries the entry lists, or `None` to keep the existing entry's.
    pub requires_bins: Option<Vec<String>>,
}

/// What the repository already holds at the skill's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub version: String,
    pub contract_changed: bool,
}

/// Why a publish is refused. Each variant is a condition the caller can fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    ListedAsExternal {
        name: String,
    },
    ListedAsWasm {
        name: String,
    },
    Prebuilt {
        name: String,
    },
    UnparseablePublishedVersion {
        name: String,
        version: String,
    },
    SameVersionDrift {
        kind: SkillKind,
        name: String,
        version: String,
        contract_changed: bool,
    },
    CatalogAhead {
        kind: SkillKind,
        name: String,
        installed: String,
        published: String,
    },
}

impl std::error::Error for Refusal {}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListedAsExternal { name } => write!(
                f,
                "The catalog lists '{name}' as an external (SKILL.md) skill; a WASM skill \
                 cannot take its entry."
            ),
            Self::ListedAsWasm { name } => write!(
                f,
                "The catalog lists '{name}' as a WASM skill; an external (SKILL.md) skill cannot \
                 take its entry."
            ),
            Self::Prebuilt { name } => write!(
                f,
                "The catalog entry for '{name}' is prebuilt (wasm_path + sha256). Publishing \
                 ships source that the hub builds on install; drop those two fields from the \
                 entry and publish again."
            ),
            Self::UnparseablePublishedVersion { name, version } => write!(
                f,
                "The catalog's version '{version}' for '{name}' is not semver, so the \
                 installed version cannot be compared with it; fix the entry first."
            ),
            Self::SameVersionDrift {
                kind: SkillKind::Wasm,
                name,
                version,
                contract_changed: true,
            } => write!(
                f,
                "'{name}' v{version} is already in the catalog with a different consent \
                 contract (tools, permissions, credentials or network): installing it over \
                 the published one would silently re-request every grant. Bump [skill] \
                 version in Skill.toml, rebuild (wasm_skill.build) and publish again."
            ),
            Self::SameVersionDrift {
                kind: SkillKind::Wasm,
                name,
                version,
                ..
            } => write!(
                f,
                "'{name}' v{version} is already in the catalog with different source. Bump \
                 [skill] version in Skill.toml, rebuild (wasm_skill.build) and publish again."
            ),
            Self::SameVersionDrift {
                kind: SkillKind::External,
                name,
                version,
                ..
            } => write!(
                f,
                "'{name}' v{version} is already in the catalog with different files. Bump \
                 version in SKILL.md, install it again (system.install_external_skill) and \
                 publish again."
            ),
            Self::CatalogAhead {
                kind: SkillKind::Wasm,
                name,
                installed,
                published,
            } => write!(
                f,
                "The catalog holds '{name}' v{published}, newer than the installed \
                 v{installed}. Install the catalog version, or bump past it and rebuild."
            ),
            Self::CatalogAhead {
                kind: SkillKind::External,
                name,
                installed,
                published,
            } => write!(
                f,
                "The catalog holds '{name}' v{published}, newer than the installed \
                 v{installed}. Install the catalog version, or bump version in SKILL.md past \
                 it, install it again and publish again."
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    pub previous_version: Option<String>,
    pub contract_changed: bool,
    pub entry: CatalogEntry,
}

/// The catalog directory a skill is published to: its existing entry's, or
/// `<manifest dir>/<kind dir>/<name>` for a new skill.
pub fn entry_subpath(
    existing: Option<&CatalogEntry>,
    manifest_path: &str,
    name: &str,
    kind: SkillKind,
) -> String {
    existing.map(|e| e.subpath.clone()).unwrap_or_else(|| {
        let dir = Path::new(manifest_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty())
            .map(|p| format!("{p}/"))
            .unwrap_or_default();
        format!("{dir}{}/{name}", kind.catalog_dir())
    })
}

/// The version gate. `published` is what the repo holds at the subpath when
/// it has a manifest there; `tree_identical` says whether the installed files
/// already match the repo tree there. `Ok(None)` means the repo already holds
/// exactly this version and these files.
pub fn plan_release(
    release: &Release,
    subpath: &str,
    existing: Option<&CatalogEntry>,
    published: Option<&Published>,
    tree_identical: bool,
) -> Result<Option<PublishPlan>, Refusal> {
    let name = release.name.clone();
    match (
        release.kind,
        existing.map(|e| (e.kind, e.builds_from_source())),
    ) {
        (SkillKind::Wasm, Some((SkillKind::External, _))) => {
            return Err(Refusal::ListedAsExternal { name })
        }
        (SkillKind::External, Some((SkillKind::Wasm, _))) => {
            return Err(Refusal::ListedAsWasm { name })
        }
        (SkillKind::Wasm, Some((_, false))) => return Err(Refusal::Prebuilt { name }),
        _ => {}
    }
    let installed_version = semver::Version::parse(&release.version).map_err(|_| {
        Refusal::UnparseablePublishedVersion {
            name: name.clone(),
            version: release.version.clone(),
        }
    })?;
    let published_version = published
        .map(|p| p.version.as_str())
        .or_else(|| existing.and_then(|e| e.version.as_deref()))
        .map(|v| {
            semver::Version::parse(v).map_err(|_| Refusal::UnparseablePublishedVersion {
                name: name.clone(),
                version: v.to_string(),
            })
        })
        .transpose()?;
    let contract_changed = published.is_some_and(|p| p.contract_changed);
    let entry = CatalogEntry {
        name: name.clone(),
        kind: release.kind,
        subpath: subpath.to_string(),
        version: Some(release.version.clone()),
        description: release.description.clone(),
        requires_bins: release.requires_bins.clone().unwrap_or_else(|| {
            existing
                .map(|e| e.requires_bins.clone())
                .unwrap_or_default()
        }),
        wasm_path: None,
        sha256: None,
    };
    let plan = |previous_version: Option<&semver::Version>| {
        Some(PublishPlan {
            previous_version: previous_version.map(ToString::to_string),
            contract_changed,
            entry: entry.clone(),
        })
    };
    match published_version {
        None => Ok(plan(None)),
        Some(ref p) if *p < installed_version => Ok(plan(Some(p))),
        Some(ref p) if *p > installed_version => Err(Refusal::CatalogAhead {
            kind: release.kind,
            name,
            installed: installed_version.to_string(),
            published: p.to_string(),
        }),
        Some(_) if tree_identical && existing == Some(&entry) => Ok(None),
        Some(ref p) if tree_identical => Ok(plan(Some(p))),
        Some(p) => Err(Refusal::SameVersionDrift {
            kind: release.kind,
            name,
            version: p.to_string(),
            contract_changed,
        }),
    }
}

// ─── the tree ───────────────────────────────────────────────────────────────

/// Governed files that differ between the skill's files and the repo's skill
/// directory. Only files the lister recognises take part, so a file it never
/// governs — a `README.md` beside WASM source, a catalog directory's own
/// dotfiles — is never touched.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeDiff {
    pub added: Vec<PathBuf>,
    pub updated: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// The files of a skill directory a publication governs, relative to it.
pub type FileLister = fn(&Path) -> Result<Vec<(PathBuf, u64)>>;

pub fn diff_trees(source: &Path, target: &Path, files: FileLister) -> Result<TreeDiff> {
    let listed = |dir: &Path| -> Result<BTreeSet<PathBuf>> {
        Ok(files(dir)?.into_iter().map(|(path, _)| path).collect())
    };
    let wanted = listed(source)?;
    let existing = match target.is_dir() {
        true => listed(target)?,
        false => BTreeSet::new(),
    };
    let mut diff = TreeDiff::default();
    for path in &wanted {
        if !existing.contains(path) {
            diff.added.push(path.clone());
        } else if std::fs::read(source.join(path))? != std::fs::read(target.join(path))? {
            diff.updated.push(path.clone());
        }
    }
    diff.removed = existing.difference(&wanted).cloned().collect();
    Ok(diff)
}

/// Copy `files`, relative paths under `from`, to the same paths under `to`.
pub fn copy_files(from: &Path, to: &Path, files: &[PathBuf]) -> Result<()> {
    files.iter().try_for_each(|path| {
        let dest = to.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from.join(path), &dest)
            .map(drop)
            .wrap_err_with(|| format!("failed to write {}", path.display()))
    })
}

pub fn apply_tree_diff(source: &Path, target: &Path, diff: &TreeDiff) -> Result<()> {
    let written: Vec<PathBuf> = diff.added.iter().chain(&diff.updated).cloned().collect();
    copy_files(source, target, &written)?;
    diff.removed.iter().try_for_each(|path| {
        std::fs::remove_file(target.join(path))
            .wrap_err_with(|| format!("failed to remove {}", path.display()))
    })
}

// ─── the pull request ───────────────────────────────────────────────────────

pub fn branch_name(skill: &str, version: &str) -> String {
    format!("skill/{skill}-v{version}")
}

fn commit_message(skill: &str, version: &str) -> String {
    format!("skill({skill}): publish v{version}")
}

pub fn version_line(release: &Release, plan: &PublishPlan) -> String {
    let previous = plan
        .previous_version
        .as_deref()
        .map(|v| format!("v{v}"))
        .unwrap_or_else(|| "new skill".to_string());
    format!("- Version: {previous} → v{}", release.version)
}

pub fn source_line(diff: &TreeDiff) -> String {
    format!(
        "- Source: {} added, {} updated, {} removed",
        diff.added.len(),
        diff.updated.len(),
        diff.removed.len()
    )
}

pub fn pr_body(release: &Release, tool: &str, lines: &[String], summary: Option<&str>) -> String {
    let notes = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n## Notes\n\n{s}\n"))
        .unwrap_or_default();
    format!(
        "Publishes `{name}` v{version} from the workforce hub (`{tool}`).\n\n{lines}\n{notes}",
        name = release.name,
        version = release.version,
        lines = lines.join("\n"),
    )
}

// ─── the repository ─────────────────────────────────────────────────────────

async fn git(remote: &dyn PublishRemote, dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = hardened_git();
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let output = cmd
        .args(args)
        .output()
        .await
        .wrap_err("failed to run git (is git installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            remote.scrub(stderr.trim())
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A shallow clone of the catalog repo at its base branch, living in the
/// caller's scratch directory.
struct Checkout {
    dir: PathBuf,
    remote: Arc<dyn PublishRemote>,
}

impl Checkout {
    async fn clone(remote: Arc<dyn PublishRemote>, dir: PathBuf, base: &str) -> Result<Self> {
        let dir_str = dir
            .to_str()
            .ok_or_else(|| eyre!("scratch path is not valid UTF-8"))?;
        git(
            remote.as_ref(),
            None,
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                "--branch",
                base,
                &remote.clone_url(),
                dir_str,
            ],
        )
        .await?;
        lock_down_config(&dir.join(".git"));
        Ok(Self { dir, remote })
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        git(self.remote.as_ref(), Some(&self.dir), args).await
    }

    async fn remote_branch_exists(&self, branch: &str) -> Result<bool> {
        let listed = self
            .run(&["ls-remote", "--heads", "origin", branch])
            .await?;
        Ok(!listed.is_empty())
    }
}

// ─── the publish ────────────────────────────────────────────────────────────

fn base_branch(source: &CatalogSource, requested: Option<&str>) -> Result<String, String> {
    match requested {
        Some(base) => Ok(base.to_string()),
        None if is_commit_sha(&source.git_ref) => Err(format!(
            "Catalog '{}' pins commit {}, which names no branch to merge into; pass `base`.",
            source.name, source.git_ref
        )),
        None => Ok(source.git_ref.clone()),
    }
}

#[derive(Deserialize)]
pub struct PublishInput {
    pub skill: String,
    #[serde(default)]
    pub catalog: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// The input every publish tool takes.
pub fn publish_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "skill": { "type": "string", "description": "Installed skill name" },
            "catalog": {
                "type": "string",
                "description": "Catalog name from [[skills.catalogs]] (optional when only one is configured)"
            },
            "base": {
                "type": "string",
                "description": "Branch to open the PR against (default: the catalog's ref; required when that ref is a commit)"
            },
            "summary": {
                "type": "string",
                "description": "Optional notes for the PR body — what changed and why"
            }
        },
        "required": ["skill"]
    })
}

/// What one kind of installed skill brings to a publish.
pub trait Publication: Sync {
    /// The tool the pull request names.
    const TOOL: &'static str;
    /// The manifest file the repository keeps in the skill's directory.
    const MANIFEST: &'static str;

    fn release(&self) -> Release;
    /// The files of a skill directory this kind governs.
    fn governed_files(dir: &Path) -> Result<Vec<(PathBuf, u64)>>;
    /// Write the installed skill's files under `scratch`, returning the tree to publish.
    fn source_tree(&self, scratch: &Path) -> Result<PathBuf>;
    fn read_published(&self, manifest: &Path) -> Result<Published>;
    fn body_lines(&self, plan: &PublishPlan, diff: &TreeDiff) -> Vec<String>;
}

/// Open a pull request publishing `publication` to the catalog `params` names.
pub async fn publish<P: Publication>(
    publication: &P,
    catalogs: &[CatalogSource],
    remote_for: &RemoteFactory,
    token_for: &TokenFor<'_>,
    params: &PublishInput,
) -> Result<serde_json::Value> {
    let release = publication.release();
    let (name, version) = (&release.name, &release.version);
    let source = select_catalog(catalogs, params.catalog.as_deref()).map_err(Report::msg)?;
    let base = base_branch(source, params.base.as_deref()).map_err(Report::msg)?;
    let remote = remote_for(source, token_for).map_err(Report::msg)?;

    let scratch = tempfile::tempdir()?;
    let repo = Checkout::clone(Arc::clone(&remote), scratch.path().join("repo"), &base).await?;
    let catalog_path = repo.dir.join(&source.manifest);
    let catalog_json = std::fs::read_to_string(&catalog_path).map_err(|_| {
        eyre!(
            "Catalog manifest {} is not at {base} of {}.",
            source.manifest,
            source.repo
        )
    })?;
    let mut manifest = CatalogManifest::parse(&catalog_json)?;
    let existing = manifest.find(name).cloned();
    let subpath = entry_subpath(existing.as_ref(), &source.manifest, name, release.kind);
    let target = repo.dir.join(&subpath);
    let manifest_path = target.join(P::MANIFEST);
    let published = manifest_path
        .is_file()
        .then(|| publication.read_published(&manifest_path))
        .transpose()
        .wrap_err_with(|| format!("The repo's {subpath}/{} does not parse", P::MANIFEST))?;

    let tree = publication.source_tree(&scratch.path().join("snapshot"))?;
    let diff = diff_trees(&tree, &target, P::governed_files)?;
    let Some(plan) = plan_release(
        &release,
        &subpath,
        existing.as_ref(),
        published.as_ref(),
        diff.is_empty(),
    )?
    else {
        return Ok(serde_json::json!({
            "skill": name,
            "version": version,
            "catalog": source.name,
            "published": false,
            "note": "The catalog already holds this version with identical source.",
        }));
    };

    let branch = branch_name(name, version);
    if repo.remote_branch_exists(&branch).await? {
        bail!(
            "Branch {branch} already exists on {}: this version was already pushed. \
             Merge or delete that branch, or bump the version.",
            source.repo
        );
    }
    apply_tree_diff(&tree, &target, &diff)?;
    manifest.upsert(plan.entry.clone());
    std::fs::write(&catalog_path, manifest.to_json_pretty()?)?;

    let title = commit_message(name, version);
    repo.run(&["checkout", "-q", "-b", &branch]).await?;
    repo.run(&["add", "-A", "--", &subpath, &source.manifest])
        .await?;
    repo.run(&["commit", "-q", "-m", &title]).await?;
    let commit = repo.run(&["rev-parse", "HEAD"]).await?;
    repo.run(&["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")])
        .await?;

    let pr = remote
        .open_pull_request(PullRequestSpec {
            title,
            head: branch.clone(),
            base: base.clone(),
            body: pr_body(
                &release,
                P::TOOL,
                &publication.body_lines(&plan, &diff),
                params.summary.as_deref(),
            ),
        })
        .await
        .wrap_err_with(|| {
            format!(
                "Branch {branch} was pushed but opening the pull request failed; open it \
                 by hand from that branch"
            )
        })?;
    info!(
        skill = %name,
        version = %version,
        catalog = %source.name,
        branch = %branch,
        pr = %pr.url,
        "{} skill published to catalog",
        kind_word(release.kind)
    );
    let mut output = serde_json::json!({
        "skill": name,
        "version": version,
        "previous_version": plan.previous_version,
        "catalog": source.name,
        "repo": source.repo,
        "subpath": subpath,
        "branch": branch,
        "base": base,
        "commit": commit,
        "pr_url": pr.url,
        "pr_number": pr.number,
        "files": {
            "added": diff.added.len(),
            "updated": diff.updated.len(),
            "removed": diff.removed.len(),
        },
        "published": true,
    });
    if release.kind == SkillKind::Wasm {
        output["contract_changed"] = serde_json::Value::Bool(plan.contract_changed);
    }
    Ok(output)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    pub(crate) const CATALOG: &str = r#"{
  "name": "test-catalog",
  "version": 1,
  "skills": [
    {
      "name": "hello",
      "kind": "external",
      "subpath": "workforce/external/hello",
      "version": "1.0.0",
      "description": "smoke"
    }
  ]
}
"#;

    pub(crate) fn catalog_source(name: &str) -> CatalogSource {
        CatalogSource {
            name: name.into(),
            repo: "https://github.com/acme/skills".into(),
            git_ref: "main".into(),
            manifest: "workforce/catalog.json".into(),
        }
    }

    pub(crate) struct LocalRemote {
        pub url: String,
        pub opened: Mutex<Vec<PullRequestSpec>>,
    }

    #[async_trait]
    impl PublishRemote for LocalRemote {
        fn clone_url(&self) -> String {
            self.url.clone()
        }

        fn scrub(&self, text: &str) -> String {
            text.to_string()
        }

        async fn open_pull_request(&self, spec: PullRequestSpec) -> Result<PullRequest> {
            let mut opened = self.opened.lock().unwrap();
            opened.push(spec);
            let number = opened.len() as u64;
            Ok(PullRequest {
                url: format!("local://pr/{number}"),
                number,
            })
        }
    }

    pub(crate) async fn sh_git(dir: &Path, args: &[&str]) -> String {
        let out = hardened_git()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare "origin" seeded with `main` holding the catalog manifest.
    pub(crate) async fn seed_origin(root: &Path) -> PathBuf {
        let bare = root.join("origin.git");
        let work = root.join("seed");
        std::fs::create_dir_all(work.join("workforce")).unwrap();
        std::fs::write(work.join("workforce/catalog.json"), CATALOG).unwrap();
        hardened_git()
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .status()
            .await
            .unwrap();
        sh_git(root, &["init", "-q", "seed"]).await;
        sh_git(&work, &["checkout", "-q", "-b", "main"]).await;
        sh_git(&work, &["add", "-A"]).await;
        sh_git(&work, &["commit", "-q", "-m", "seed"]).await;
        sh_git(
            &work,
            &[
                "remote",
                "add",
                "origin",
                &format!("file://{}", bare.display()),
            ],
        )
        .await;
        sh_git(&work, &["push", "-q", "origin", "main"]).await;
        bare
    }

    fn external_release(bins: Option<Vec<String>>) -> Release {
        Release {
            kind: SkillKind::External,
            name: "hello-world".into(),
            version: "1.1.0".into(),
            description: "Says hello".into(),
            requires_bins: bins,
        }
    }

    fn entry(kind: SkillKind, version: &str, bins: &[&str]) -> CatalogEntry {
        CatalogEntry {
            name: "hello-world".into(),
            kind,
            subpath: format!("workforce/{}/hello-world", kind.catalog_dir()),
            version: Some(version.into()),
            description: "Says hello".into(),
            requires_bins: bins.iter().map(ToString::to_string).collect(),
            wasm_path: None,
            sha256: None,
        }
    }

    #[test]
    fn base_branch_defaults_to_the_ref_unless_it_is_a_commit() {
        let mut source = catalog_source("a");
        assert_eq!(base_branch(&source, None).unwrap(), "main");
        assert_eq!(base_branch(&source, Some("release")).unwrap(), "release");
        source.git_ref = "0123456789abcdef0123456789abcdef01234567".into();
        assert!(base_branch(&source, None)
            .unwrap_err()
            .contains("pass `base`"));
        assert_eq!(base_branch(&source, Some("main")).unwrap(), "main");
    }

    #[test]
    fn github_remote_scrubs_its_token() {
        let remote = GithubRemote {
            owner_repo: "acme/skills".into(),
            token: "ghp_secret".into(),
        };
        assert_eq!(
            remote.clone_url(),
            "https://x-access-token:ghp_secret@github.com/acme/skills.git"
        );
        assert_eq!(
            remote.scrub("push to ghp_secret failed"),
            "push to *** failed"
        );
    }

    #[test]
    fn external_subpath_and_refusal_texts() {
        assert_eq!(
            entry_subpath(
                None,
                "workforce/catalog.json",
                "hello-world",
                SkillKind::External
            ),
            "workforce/external/hello-world"
        );
        assert_eq!(
            entry_subpath(None, "catalog.json", "hello-world", SkillKind::External),
            "external/hello-world"
        );
        let release = external_release(None);
        let wasm = entry(SkillKind::Wasm, "1.0.0", &[]);
        assert_eq!(
            plan_release(&release, &wasm.subpath, Some(&wasm), None, false)
                .unwrap_err()
                .to_string(),
            "The catalog lists 'hello-world' as a WASM skill; an external (SKILL.md) skill \
             cannot take its entry."
        );
        let same = entry(SkillKind::External, "1.1.0", &[]);
        let drift = plan_release(&release, &same.subpath, Some(&same), None, false).unwrap_err();
        assert_eq!(
            drift,
            Refusal::SameVersionDrift {
                kind: SkillKind::External,
                name: "hello-world".into(),
                version: "1.1.0".into(),
                contract_changed: false,
            }
        );
        assert_eq!(
            drift.to_string(),
            "'hello-world' v1.1.0 is already in the catalog with different files. Bump version \
             in SKILL.md, install it again (system.install_external_skill) and publish again."
        );
        let ahead = entry(SkillKind::External, "2.0.0", &[]);
        assert_eq!(
            plan_release(&release, &ahead.subpath, Some(&ahead), None, false)
                .unwrap_err()
                .to_string(),
            "The catalog holds 'hello-world' v2.0.0, newer than the installed v1.1.0. Install \
             the catalog version, or bump version in SKILL.md past it, install it again and \
             publish again."
        );
        let older = entry(SkillKind::External, "1.0.0", &[]);
        assert!(
            plan_release(&release, &older.subpath, Some(&older), None, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn plan_release_takes_bins_from_an_external_release() {
        let listed = entry(SkillKind::External, "1.0.0", &["jq"]);
        let plan = plan_release(
            &external_release(Some(vec!["git".into()])),
            &listed.subpath,
            Some(&listed),
            Some(&Published {
                version: "1.0.0".into(),
                contract_changed: false,
            }),
            false,
        )
        .unwrap()
        .expect("a publish");
        assert_eq!(plan.entry.requires_bins, vec!["git".to_string()]);
        assert_eq!(plan.entry.kind, SkillKind::External);
        assert_eq!(plan.previous_version.as_deref(), Some("1.0.0"));
        assert!(!plan.contract_changed);

        let kept = plan_release(
            &Release {
                kind: SkillKind::Wasm,
                ..external_release(None)
            },
            "workforce/wasm/hello-world",
            Some(&entry(SkillKind::Wasm, "1.0.0", &["jq"])),
            None,
            false,
        )
        .unwrap()
        .expect("a publish");
        assert_eq!(kept.entry.requires_bins, vec!["jq".to_string()]);
    }
}
