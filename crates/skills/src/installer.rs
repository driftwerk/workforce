//! Skill installer — find a skill in a git repository or a local directory, and install or uninstall it.
//!
//! Skills are installed to the workspace `skills/` directory. Git-based skills
//! are cloned, and local skills are copied.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

/// The file in an installed skill's directory recording who owns the skill and who may use it.
/// Only the hub writes one; a copy from any source leaves it behind.
pub const ACCESS_FILE: &str = ".workforce-access.json";

/// Options controlling a git-based install.
#[derive(Debug, Clone, Default)]
pub struct InstallOptions<'a> {
    /// GitHub token. When set, `https://…` clone URLs are authenticated as
    /// `x-access-token` so private repos resolve; ssh URLs are left untouched.
    /// Only pass a GitHub token with a github.com URL: it goes to whichever host an https URL names.
    pub token: Option<&'a str>,
    /// Branch, tag, or full 40-hex commit to install. `None` = default branch.
    pub git_ref: Option<&'a str>,
}

/// A full 40-hex git commit id, as opposed to a branch or tag name.
pub fn is_commit_sha(git_ref: &str) -> bool {
    git_ref.len() == 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}

/// `git clone` exited unsuccessfully: git's stderr, trimmed, with any token redacted.
#[derive(Debug)]
pub struct CloneFailed(pub String);

impl std::fmt::Display for CloneFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git clone failed: {}", self.0)
    }
}

impl std::error::Error for CloneFailed {}

/// `owner/repo[@ref][#subpath=dir]`, the `github_repo` that `wasm_skill.install` and
/// `system.install_external_skill` both take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubRef<'a> {
    pub repo: &'a str,
    pub git_ref: Option<&'a str>,
    pub subpath: Option<&'a str>,
}

impl<'a> GithubRef<'a> {
    pub fn parse(reference: &'a str) -> Self {
        let (head, subpath) = split_subpath(reference);
        let (repo, git_ref) = head
            .split_once('@')
            .map_or((head, None), |(repo, git_ref)| (repo, Some(git_ref)));
        Self {
            repo,
            git_ref,
            subpath,
        }
    }

    pub fn owner(&self) -> &'a str {
        self.repo
            .split_once('/')
            .map_or(self.repo, |(owner, _)| owner)
    }

    /// An owner and a repository name spelt with the characters GitHub allows, and no empty ref.
    pub fn is_well_formed(&self) -> bool {
        let is_name = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        self.git_ref != Some("")
            && self.repo.split_once('/').is_some_and(|(owner, name)| {
                is_name(owner) && is_name(name) && !name.ends_with(".git")
            })
    }
}

impl std::fmt::Display for GithubRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.repo)?;
        if let Some(git_ref) = self.git_ref {
            write!(f, "@{git_ref}")?;
        }
        if let Some(subpath) = self.subpath {
            write!(f, "#subpath={subpath}")?;
        }
        Ok(())
    }
}

/// Where `system.install_external_skill` reads a skill from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin<'a> {
    /// A github.com repository, read with a GitHub token when one is set.
    Github(GithubRef<'a>),
    /// Any other git URL, cloned without credentials.
    Git(&'a str),
    /// A directory on this host.
    Local(&'a str),
}

impl<'a> SkillOrigin<'a> {
    pub fn from_source(source: &'a str) -> Self {
        if !is_git_url(source) {
            return Self::Local(source);
        }
        let (url, subpath) = split_subpath(source);
        url.strip_prefix("https://github.com/")
            .map(|path| path.trim_end_matches('/'))
            .map(|path| path.strip_suffix(".git").unwrap_or(path))
            .map(|repo| GithubRef {
                repo,
                git_ref: None,
                subpath,
            })
            .filter(GithubRef::is_well_formed)
            .map_or(Self::Git(source), Self::Github)
    }

    pub fn locate_args(&self) -> (String, Option<&'a str>) {
        match self {
            Self::Github(github) => {
                let fragment = github
                    .subpath
                    .map(|dir| format!("#subpath={dir}"))
                    .unwrap_or_default();
                (
                    format!("https://github.com/{}.git{fragment}", github.repo),
                    github.git_ref,
                )
            }
            Self::Git(source) | Self::Local(source) => (source.to_string(), None),
        }
    }
}

/// Install a skill from a source (git URL or local path) into the target directory.
///
/// Returns the path to the installed skill directory.
pub fn install_skill(source: &str, target_dir: &Path) -> eyre::Result<PathBuf> {
    install_skill_with(source, target_dir, &InstallOptions::default())
}

/// Install a skill, authenticating and pinning the ref per [`InstallOptions`].
pub fn install_skill_with(
    source: &str,
    target_dir: &Path,
    opts: &InstallOptions,
) -> eyre::Result<PathBuf> {
    locate_skill(source, opts)
        .and_then(|located| place_skill(&located, target_dir, Placement::Fresh, None))
        .map(|installed| installed.path)
}

/// Manifest kind discovered in a skill source directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    /// `SKILL.md` markdown skill (instruction-based).
    Markdown,
    /// `Skill.toml` WASM skill (compiled).
    Wasm,
}

/// A skill found in its source, ready to place. A git checkout lives as long as this value.
pub struct SkillSource {
    root: PathBuf,
    name: String,
    dir_name: String,
    kind: ManifestKind,
    commit: Option<String>,
    _checkout: Option<tempfile::TempDir>,
}

impl SkillSource {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory name the skill is placed under.
    pub fn dir_name(&self) -> &str {
        &self.dir_name
    }

    pub fn kind(&self) -> ManifestKind {
        self.kind
    }

    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }
}

/// Whether [`place_skill`] may overwrite a skill already in the target directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Refuse when the destination exists.
    Fresh,
    /// Swap the new copy in for an existing install of the same skill, keeping its `config.toml`.
    Replace,
}

/// Where [`place_skill`] put a skill, and what it replaced.
#[derive(Debug)]
pub struct Installed {
    pub path: PathBuf,
    pub replaced: bool,
    pub previous_version: Option<String>,
}

/// Find the skill at `source` (git URL or local path) without writing anything.
pub fn locate_skill(source: &str, opts: &InstallOptions) -> eyre::Result<SkillSource> {
    match is_git_url(source) {
        true => locate_git(source, opts),
        false => locate_local(Path::new(source)),
    }
}

/// Copy a located skill into `target_dir`.
///
/// The copy is staged in a hidden scratch directory inside `target_dir` and moved into place with
/// renames, so a scan never sees a half-written skill and a failed replace restores the previous
/// install. The staged skill sits one level below the scratch directory, which therefore holds no
/// `SKILL.md` of its own and is invisible to every skill scan.
///
/// `access` becomes the placed skill's [`ACCESS_FILE`]; without one the placed copy has none,
/// whatever the replaced copy had.
pub fn place_skill(
    source: &SkillSource,
    target_dir: &Path,
    placement: Placement,
    access: Option<&str>,
) -> eyre::Result<Installed> {
    std::fs::create_dir_all(target_dir).map_err(|e| {
        eyre::eyre!(
            "Failed to create skills directory {}: {}",
            target_dir.display(),
            e
        )
    })?;
    let name = &source.name;
    if !is_directory_name(&source.dir_name) {
        eyre::bail!(
            "Skill '{name}' would install to '{}', which is not a directory name",
            source.dir_name
        );
    }
    let dest = target_dir.join(&source.dir_name);

    if let Some(other) = skill_dirs_named(name, target_dir)?
        .into_iter()
        .find(|dir| *dir != dest)
    {
        eyre::bail!(
            "Skill '{name}' is already installed in '{}'. Uninstall it first.",
            other.display()
        );
    }
    let existing = dest.exists();
    match (existing, placement) {
        (true, Placement::Fresh) => eyre::bail!(
            "Skill directory '{}' already exists. Uninstall first or use a different name.",
            dest.display()
        ),
        (true, Placement::Replace) => {
            if let Some(other) = manifest_name(&dest).filter(|other| other != name) {
                eyre::bail!(
                    "Skill directory '{}' holds skill '{other}', not '{name}'. Uninstall it first.",
                    dest.display()
                );
            }
        }
        (false, _) => {}
    }
    let previous_version = existing.then(|| installed_version(&dest)).flatten();

    let scratch = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(target_dir)
        .map_err(|e| {
            eyre::eyre!(
                "Failed to create a staging directory in {}: {}",
                target_dir.display(),
                e
            )
        })?;
    let staged = scratch.path().join("skill");
    copy_dir_recursive(&source.root, &staged)?;
    record_access(&staged, access)
        .map_err(|e| eyre::eyre!("Failed to record who owns '{name}': {e}"))?;

    match existing {
        false => std::fs::rename(&staged, &dest)
            .map_err(|e| eyre::eyre!("Failed to move '{name}' into place: {e}"))?,
        true => swap_into_place(name, scratch, &staged, &dest)?,
    }

    info!(
        name = %name,
        kind = ?source.kind,
        path = %dest.display(),
        replaced = existing,
        "Skill installed"
    );
    Ok(Installed {
        path: dest,
        replaced: existing,
        previous_version,
    })
}

/// Make `access` the staged skill's only access record. Whatever [`ACCESS_FILE`] resolves to is
/// removed first: on a case-insensitive filesystem that is a source's differently cased copy,
/// which the copy does not recognise.
fn record_access(staged: &Path, access: Option<&str>) -> std::io::Result<()> {
    let path = staged.join(ACCESS_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path)?,
        Ok(_) => std::fs::remove_file(&path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    access.map_or(Ok(()), |record| std::fs::write(&path, record))
}

/// Replace `dest` with `staged`, carrying the operator's `config.toml` over any shipped default.
/// On failure the previous install is moved back, or kept inside the scratch directory and named
/// in the error when even that fails.
fn swap_into_place(
    name: &str,
    scratch: tempfile::TempDir,
    staged: &Path,
    dest: &Path,
) -> eyre::Result<()> {
    let config = dest.join("config.toml");
    if config.is_file() {
        std::fs::copy(&config, staged.join("config.toml"))
            .map_err(|e| eyre::eyre!("Failed to carry config.toml forward for '{name}': {e}"))?;
    }
    let previous = scratch.path().join("previous");
    std::fs::rename(dest, &previous)
        .map_err(|e| eyre::eyre!("Failed to move the installed '{name}' aside: {e}"))?;
    std::fs::rename(staged, dest).map_err(|e| match std::fs::rename(&previous, dest) {
        Ok(()) => eyre::eyre!(
            "Failed to move the new '{name}' into place: {e}. The previous install was restored."
        ),
        Err(restore) => eyre::eyre!(
            "Failed to move the new '{name}' into place: {e}, and restoring the previous install \
             failed: {restore}. It is kept at '{}'.",
            scratch.keep().join("previous").display()
        ),
    })
}

/// The `version` in `skill_dir/SKILL.md`, when it parses.
pub fn installed_version(skill_dir: &Path) -> Option<String> {
    read_skill_md(skill_dir).map(|skill| skill.meta.version)
}

/// Remove an installed skill by name from the target directory.
pub fn uninstall_skill(name: &str, target_dir: &Path) -> eyre::Result<()> {
    find_skill_dir_by_name(name, target_dir)
        .and_then(|skill_dir| remove_skill_dir(name, &skill_dir))
}

/// Remove `skill_dir`, the directory [`find_skill_dir_by_name`] found for `name`. One rename takes
/// it out of every scan's sight before anything in it is deleted, so no reader finds the skill
/// with part of it gone: its [`ACCESS_FILE`] deleted first would leave a private skill looking
/// like everyone's.
pub fn remove_skill_dir(name: &str, skill_dir: &Path) -> eyre::Result<()> {
    let failed = |e: std::io::Error| {
        eyre::eyre!(
            "Failed to remove skill directory {}: {}",
            skill_dir.display(),
            e
        )
    };
    let parent = skill_dir
        .parent()
        .ok_or_else(|| failed(std::io::ErrorKind::InvalidInput.into()))?;
    let scratch = tempfile::Builder::new()
        .prefix(".remove-")
        .tempdir_in(parent)
        .map_err(failed)?;
    std::fs::rename(skill_dir, scratch.path().join("skill")).map_err(failed)?;
    scratch.close().map_err(failed)?;
    info!(name = %name, path = %skill_dir.display(), "Skill uninstalled");
    Ok(())
}

fn read_skill_md(dir: &Path) -> Option<crate::skill_file::SkillFile> {
    std::fs::read_to_string(dir.join("SKILL.md"))
        .ok()
        .and_then(|content| crate::skill_file::SkillFile::parse(&content).ok())
}

fn manifest_name(dir: &Path) -> Option<String> {
    read_skill_md(dir).map(|skill| skill.meta.name)
}

/// Every directory in `target_dir` whose `SKILL.md` names `name`, sorted by path.
fn skill_dirs_named(name: &str, target_dir: &Path) -> eyre::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(target_dir)
        .map_err(|e| eyre::eyre!("Failed to read skills directory: {}", e))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| manifest_name(dir).is_some_and(|found| found == name))
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Whether `name` is one directory name: a single path component, not `.` or `..`.
pub fn is_directory_name(name: &str) -> bool {
    !name.contains(['/', '\\'])
        && matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        )
}

/// The directory [`uninstall_skill`] removes for `name`: the first whose `SKILL.md` names it, else
/// the directory in `target_dir` called `name` that holds a `SKILL.md`.
pub fn find_skill_dir_by_name(name: &str, target_dir: &Path) -> eyre::Result<PathBuf> {
    if !target_dir.is_dir() {
        return Err(eyre::eyre!(
            "Skills directory {} does not exist",
            target_dir.display()
        ));
    }

    skill_dirs_named(name, target_dir)?
        .into_iter()
        .next()
        .or_else(|| {
            let direct = target_dir.join(name);
            (is_directory_name(name) && direct.join("SKILL.md").is_file()).then_some(direct)
        })
        .ok_or_else(|| eyre::eyre!("Skill '{}' not found in {}", name, target_dir.display()))
}

/// Find a skill in a git repo.
///
/// Supports a `#subpath=<path>` URL fragment for mono-repos that house
/// multiple skills (e.g. `https://github.com/acme/skills#subpath=hello-world`).
/// Without a fragment, the skill manifest must be at the clone root.
fn locate_git(url: &str, opts: &InstallOptions) -> eyre::Result<SkillSource> {
    let (clean_url, subpath) = split_subpath(url);
    if let Some(p) = subpath {
        validate_subpath(p)?;
    }

    let tmp = tempfile::tempdir()
        .map_err(|e| eyre::eyre!("Failed to create tempdir for git clone: {}", e))?;
    let clone_dir = tmp.path().join("clone");

    info!(url = %clean_url, git_ref = ?opts.git_ref, subpath = ?subpath, "Cloning skill from git");

    let clone_url = authenticated_clone_url(clean_url, opts.token);
    // `--branch` takes only branch and tag names, so a commit pin clones in
    // full and checks the exact commit out; any other ref is shallow-cloned.
    let commit_pin = opts.git_ref.filter(|r| is_commit_sha(r));
    let mut args = match commit_pin {
        Some(_) => vec!["clone"],
        None => {
            let mut args = vec!["clone", "--depth", "1"];
            if let Some(git_ref) = opts.git_ref {
                args.extend(["--branch", git_ref]);
            }
            args
        }
    };
    args.push(&clone_url);

    let output = Command::new("git")
        .args(&args)
        .arg(&clone_dir)
        .output()
        .map_err(|e| eyre::eyre!("Failed to run git clone: {} (is git installed?)", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CloneFailed(redact_token(&stderr, opts.token).trim().to_string()).into());
    }
    let commit = match commit_pin {
        Some(sha) => checkout_commit(&clone_dir, sha)?,
        None => head_commit(&clone_dir)?,
    };

    let skill_root = match subpath {
        Some(p) => clone_dir.join(p),
        None => clone_dir.clone(),
    };
    if !skill_root.is_dir() {
        return Err(eyre::eyre!(
            "Subpath '{}' not found in cloned repository",
            subpath.unwrap_or("")
        ));
    }

    let (kind, name) = find_manifest_in(&skill_root)?;
    Ok(SkillSource {
        root: skill_root,
        dir_name: name.clone(),
        name,
        kind,
        commit: Some(commit),
        _checkout: Some(tmp),
    })
}

/// Check `sha` out in the clone at `repo`, then prove the working tree is that
/// commit: a pin is only worth anything if it is verified, not trusted from
/// the checkout's exit code.
fn checkout_commit(repo: &Path, sha: &str) -> eyre::Result<String> {
    let checkout = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["checkout", "--detach", sha])
        .output()
        .map_err(|e| eyre::eyre!("Failed to run git checkout: {}", e))?;
    if !checkout.status.success() {
        return Err(eyre::eyre!(
            "git checkout of pinned commit {} failed: {}",
            sha,
            String::from_utf8_lossy(&checkout.stderr).trim()
        ));
    }

    let head = head_commit(repo)?;
    eyre::ensure!(
        head == sha.to_lowercase(),
        "pinned commit mismatch: requested {sha}, checkout produced {head}"
    );
    Ok(head)
}

fn head_commit(repo: &Path) -> eyre::Result<String> {
    let head = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| eyre::eyre!("Failed to run git rev-parse: {}", e))?;
    eyre::ensure!(
        head.status.success(),
        "git rev-parse HEAD failed: {}",
        String::from_utf8_lossy(&head.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&head.stdout).trim().to_lowercase())
}

/// Look for a skill manifest at `dir`. Returns the kind and the skill's
/// name (for naming the install destination). Used by both git and local
/// install paths so mono-repos and single-skill repos behave the same.
fn find_manifest_in(dir: &Path) -> eyre::Result<(ManifestKind, String)> {
    let skill_md = dir.join("SKILL.md");
    let skill_toml = dir.join("Skill.toml");
    if skill_md.is_file() {
        let content = std::fs::read_to_string(&skill_md)
            .map_err(|e| eyre::eyre!("Failed to read SKILL.md: {}", e))?;
        let skill = crate::skill_file::SkillFile::parse(&content)
            .map_err(|e| eyre::eyre!("Invalid SKILL.md at {}: {}", skill_md.display(), e))?;
        Ok((ManifestKind::Markdown, skill.meta.name))
    } else if skill_toml.is_file() {
        // WASM-skill manifest. Parsed via the wasm-runtime crate's
        // SkillManifest. We don't depend on that crate here just for
        // installation, so we extract the `name` via a minimal serde
        // deserialiser that ignores everything else.
        let content = std::fs::read_to_string(&skill_toml)
            .map_err(|e| eyre::eyre!("Failed to read Skill.toml: {}", e))?;
        #[derive(serde::Deserialize)]
        struct MinimalManifest {
            skill: MinimalSkill,
        }
        #[derive(serde::Deserialize)]
        struct MinimalSkill {
            name: String,
        }
        let parsed: MinimalManifest = toml::from_str(&content)
            .map_err(|e| eyre::eyre!("Invalid Skill.toml at {}: {}", skill_toml.display(), e))?;
        Ok((ManifestKind::Wasm, parsed.skill.name))
    } else {
        Err(eyre::eyre!(
            "No SKILL.md or Skill.toml found in '{}'",
            dir.display()
        ))
    }
}

/// Split a source at its `#` fragment into what precedes it and the fragment's `subpath=`.
///
/// `https://github.com/acme/skills.git#subpath=hello-world` → `(url, Some("hello-world"))`.
/// `https://github.com/acme/skills.git` → `(url, None)`.
pub fn split_subpath(source: &str) -> (&str, Option<&str>) {
    match source.split_once('#') {
        Some((head, fragment)) => (
            head,
            fragment
                .split('&')
                .find_map(|kv| kv.strip_prefix("subpath="))
                .map(|s| s.trim_matches('/'))
                .filter(|s| !s.is_empty()),
        ),
        None => (source, None),
    }
}

/// Reject subpaths that would escape the clone directory or otherwise
/// resolve outside the intended skill root: `..` segments, absolute
/// paths, backslash separators, and embedded NULs. Returns `Ok(())` on
/// safe paths so the caller can join the subpath with confidence.
pub fn validate_subpath(subpath: &str) -> eyre::Result<()> {
    if subpath.starts_with('/') || subpath.starts_with('\\') {
        return Err(eyre::eyre!(
            "subpath '{}' must be relative (no leading slash)",
            subpath
        ));
    }
    if subpath.contains('\\') || subpath.contains('\0') {
        return Err(eyre::eyre!(
            "subpath '{}' contains invalid characters",
            subpath
        ));
    }
    if subpath.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(eyre::eyre!(
            "subpath '{}' must not contain '..' or '.' segments",
            subpath
        ));
    }
    Ok(())
}

/// Find a skill in a local directory; it installs under the directory's own name.
fn locate_local(source: &Path) -> eyre::Result<SkillSource> {
    if !source.is_dir() {
        return Err(eyre::eyre!(
            "Source path '{}' is not a directory",
            source.display()
        ));
    }

    let (kind, name) = find_manifest_in(source)?;

    let dir_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| eyre::eyre!("Cannot determine directory name from source path"))?
        .to_string();

    Ok(SkillSource {
        root: source.to_path_buf(),
        name,
        dir_name,
        kind,
        commit: None,
        _checkout: None,
    })
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> eyre::Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| eyre::eyre!("Failed to create directory {}: {}", dst.display(), e))?;

    for entry in std::fs::read_dir(src)
        .map_err(|e| eyre::eyre!("Failed to read directory {}: {}", src.display(), e))?
    {
        let entry = entry.map_err(|e| eyre::eyre!("Failed to read directory entry: {}", e))?;
        // A checkout's .git/config keeps the clone URL, token included; an installed skill is a copy.
        // The access record is the hub's, never a source's.
        if entry.file_name() == ".git" || entry.file_name() == ACCESS_FILE {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        // audit W4: never follow symlinks. A malicious skill repo could symlink
        // to an absolute host path (/etc/..., the vault key file) and have its
        // contents copied into the installed skill dir. `file_type()` reports
        // the link itself (unlike `is_dir()`, which traverses it).
        let file_type = entry
            .file_type()
            .map_err(|e| eyre::eyre!("Failed to stat directory entry: {}", e))?;
        if file_type.is_symlink() {
            tracing::warn!(path = %src_path.display(), "skipping symlink during skill copy");
            continue;
        }

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                eyre::eyre!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                )
            })?;
        }
    }
    Ok(())
}

/// Check if a source string looks like a git URL, ignoring any `#fragment`.
fn is_git_url(source: &str) -> bool {
    let source = source.split_once('#').map_or(source, |(url, _)| url);
    source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("git@")
        || source.starts_with("git://")
        || source.ends_with(".git")
}

/// Rewrite an `https://` clone URL to authenticate as `x-access-token` so
/// private GitHub repos resolve. ssh / non-https URLs are returned unchanged.
fn authenticated_clone_url(url: &str, token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() && url.starts_with("https://") => {
            url.replacen("https://", &format!("https://x-access-token:{t}@"), 1)
        }
        _ => url.to_string(),
    }
}

/// Redact a token from text (e.g. git stderr that echoes the clone URL).
fn redact_token(text: &str, token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() => text.replace(t, "***"),
        _ => text.to_string(),
    }
}

/// Extract the repository name from a git URL. Retained as a utility for
/// callers that want a friendly default install dir name; the git
/// installer itself derives the install dir from the manifest's `name`.
#[allow(dead_code)]
fn extract_repo_name(url: &str) -> eyre::Result<String> {
    let name = url
        .rsplit('/')
        .next()
        .ok_or_else(|| eyre::eyre!("Cannot extract repository name from URL: {}", url))?
        .trim_end_matches(".git");

    if name.is_empty() {
        return Err(eyre::eyre!(
            "Cannot extract repository name from URL: {}",
            url
        ));
    }

    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_skill_md() -> &'static str {
        r#"---
name: test-skill
version: 1.0.0
description: A test skill
tools:
  - test.run
---
# Test Skill

Instructions here.
"#
    }

    #[test]
    fn is_git_url_detection() {
        assert!(is_git_url("https://github.com/user/repo.git"));
        assert!(is_git_url("git@github.com:user/repo.git"));
        assert!(is_git_url("http://example.com/repo"));
        assert!(is_git_url("git://example.com/repo"));
        assert!(is_git_url("my-repo.git"));
        assert!(!is_git_url("/home/user/skills/my-skill"));
        assert!(!is_git_url("./local-skill"));
        assert!(!is_git_url("relative/path"));
        assert!(is_git_url("/tmp/skills.git#subpath=hello-world"));
    }

    #[test]
    fn authenticated_clone_url_injects_token_for_https() {
        assert_eq!(
            authenticated_clone_url("https://github.com/o/r.git", Some("ghp_x")),
            "https://x-access-token:ghp_x@github.com/o/r.git"
        );
    }

    #[test]
    fn authenticated_clone_url_leaves_ssh_and_tokenless_untouched() {
        assert_eq!(
            authenticated_clone_url("git@github.com:o/r.git", Some("ghp_x")),
            "git@github.com:o/r.git"
        );
        assert_eq!(
            authenticated_clone_url("https://github.com/o/r.git", None),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            authenticated_clone_url("https://github.com/o/r.git", Some("")),
            "https://github.com/o/r.git"
        );
    }

    #[test]
    fn redact_token_scrubs_secret() {
        assert_eq!(
            redact_token(
                "clone https://x-access-token:ghp_x@github.com failed",
                Some("ghp_x")
            ),
            "clone https://x-access-token:***@github.com failed"
        );
        assert_eq!(redact_token("no secret here", None), "no secret here");
    }

    #[test]
    fn install_from_local_accepts_skill_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("wasm-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("Skill.toml"),
            "[skill]\nname = \"w\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[[tools]]\nname = \"p\"\ndescription = \"p\"\n",
        )
        .unwrap();

        let target = tmp.path().join("installed");
        let dest =
            install_skill(source.to_str().unwrap(), &target).expect("installs wasm skill dir");
        assert!(dest.join("Skill.toml").is_file());
    }

    #[test]
    fn extract_repo_name_from_url() {
        assert_eq!(
            extract_repo_name("https://github.com/user/my-skill.git").unwrap(),
            "my-skill"
        );
        assert_eq!(
            extract_repo_name("https://github.com/user/my-skill").unwrap(),
            "my-skill"
        );
        assert_eq!(
            extract_repo_name("git@github.com:user/skill-name.git").unwrap(),
            "skill-name"
        );
    }

    #[test]
    fn install_from_local_path() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a source skill directory
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), sample_skill_md()).unwrap();
        fs::write(source.join("extra.txt"), "extra file").unwrap();

        // Install it
        let target = tmp.path().join("installed");
        let result = install_skill(source.to_str().unwrap(), &target);
        assert!(result.is_ok());

        let installed_path = result.unwrap();
        assert!(installed_path.join("SKILL.md").is_file());
        assert!(installed_path.join("extra.txt").is_file());
    }

    #[test]
    fn split_subpath_no_fragment() {
        let (url, sub) = split_subpath("https://github.com/foo/bar.git");
        assert_eq!(url, "https://github.com/foo/bar.git");
        assert_eq!(sub, None);
    }

    #[test]
    fn split_subpath_with_subpath() {
        let (url, sub) = split_subpath("https://github.com/acme/skills#subpath=hello-world");
        assert_eq!(url, "https://github.com/acme/skills");
        assert_eq!(sub, Some("hello-world"));
    }

    #[test]
    fn split_subpath_strips_subpath_slashes() {
        let (_, sub) = split_subpath("https://x/y#subpath=/orbit/");
        assert_eq!(sub, Some("orbit"));
    }

    #[test]
    fn split_subpath_subpath_in_compound_fragment() {
        let (_, sub) = split_subpath("https://x/y#ref=main&subpath=hello-world");
        assert_eq!(sub, Some("hello-world"));
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn github_ref_round_trips_the_forms_agents_use() {
        let pinned = format!("acme/skills@{COMMIT}#subpath=hello-counter");
        for form in [
            "acme/skills",
            "acme/skills@main",
            pinned.as_str(),
            "acme/skills#subpath=hello-world",
        ] {
            assert_eq!(GithubRef::parse(form).to_string(), form);
        }
        assert_eq!(
            GithubRef::parse(&pinned),
            GithubRef {
                repo: "acme/skills",
                git_ref: Some(COMMIT),
                subpath: Some("hello-counter"),
            }
        );
    }

    #[test]
    fn github_ref_owner_and_shape() {
        assert_eq!(GithubRef::parse("acme/skills@main").owner(), "acme");
        for well_formed in [
            "acme/skills",
            "Acme-Org/skills_v2.rs@main#subpath=hello-world",
        ] {
            assert!(
                GithubRef::parse(well_formed).is_well_formed(),
                "{well_formed}"
            );
        }
        for malformed in [
            "acme",
            "acme/skills/extra",
            "/skills",
            "acme/",
            "https://github.com/acme/skills",
            "acme/skills@",
            "acme/skills.git",
            "acme/skills?tab=readme",
            "ac me/skills",
        ] {
            assert!(!GithubRef::parse(malformed).is_well_formed(), "{malformed}");
        }
    }

    #[test]
    fn skill_origin_from_source_classifies_each_form() {
        assert_eq!(
            SkillOrigin::from_source("https://github.com/acme/skills.git#subpath=hello-world"),
            SkillOrigin::Github(GithubRef {
                repo: "acme/skills",
                git_ref: None,
                subpath: Some("hello-world"),
            })
        );
        assert_eq!(
            SkillOrigin::from_source("https://github.com/acme/skills/"),
            SkillOrigin::Github(GithubRef::parse("acme/skills"))
        );
        for git in [
            "git@github.com:acme/skills.git",
            "https://gitlab.example.com/acme/skills.git",
            "https://github.com/acme",
            "/tmp/skills.git#subpath=x",
            "http://github.com/acme/skills.git",
            "https://github.com.evil.example/acme/skills.git",
            "https://evil.example/github.com/acme/skills.git",
            "https://user@github.com/acme/skills.git",
            "https://github.com/acme/skills@evil.example",
        ] {
            assert_eq!(SkillOrigin::from_source(git), SkillOrigin::Git(git));
        }
        assert_eq!(
            SkillOrigin::from_source("/srv/skills/hello-world"),
            SkillOrigin::Local("/srv/skills/hello-world")
        );
    }

    #[test]
    fn skill_origin_locate_args() {
        let pinned = format!("acme/skills@{COMMIT}#subpath=hello-world");
        assert_eq!(
            SkillOrigin::Github(GithubRef::parse(&pinned)).locate_args(),
            (
                "https://github.com/acme/skills.git#subpath=hello-world".to_string(),
                Some(COMMIT)
            )
        );
        assert_eq!(
            SkillOrigin::Github(GithubRef::parse("acme/skills")).locate_args(),
            ("https://github.com/acme/skills.git".to_string(), None)
        );
        let git = "https://gitlab.example.com/acme/skills.git#subpath=hello-world";
        assert_eq!(SkillOrigin::Git(git).locate_args(), (git.to_string(), None));
        assert_eq!(
            SkillOrigin::Local("/srv/skills/hello-world").locate_args(),
            ("/srv/skills/hello-world".to_string(), None)
        );
    }

    #[test]
    fn validate_subpath_accepts_safe_paths() {
        assert!(validate_subpath("orbit").is_ok());
        assert!(validate_subpath("nested/sub/dir").is_ok());
        assert!(validate_subpath("hello-world").is_ok());
    }

    #[test]
    fn validate_subpath_rejects_traversal() {
        assert!(validate_subpath("..").is_err());
        assert!(validate_subpath("../etc").is_err());
        assert!(validate_subpath("a/../b").is_err());
        assert!(validate_subpath("a/./b").is_err());
        assert!(validate_subpath(".").is_err());
    }

    #[test]
    fn validate_subpath_rejects_absolute_paths() {
        assert!(validate_subpath("/etc").is_err());
        assert!(validate_subpath("\\windows").is_err());
    }

    #[test]
    fn validate_subpath_rejects_invalid_chars() {
        assert!(validate_subpath("nul\0byte").is_err());
        assert!(validate_subpath("back\\slash").is_err());
    }

    #[test]
    fn find_manifest_finds_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("SKILL.md"), sample_skill_md()).unwrap();
        let (kind, name) = find_manifest_in(tmp.path()).unwrap();
        assert_eq!(kind, ManifestKind::Markdown);
        assert_eq!(name, "test-skill");
    }

    #[test]
    fn find_manifest_finds_skill_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Skill.toml"),
            r#"
[skill]
name = "wasm-thing"
version = "0.1.0"
description = "test"

[[tools]]
name = "ping"
description = "ping"
"#,
        )
        .unwrap();
        let (kind, name) = find_manifest_in(tmp.path()).unwrap();
        assert_eq!(kind, ManifestKind::Wasm);
        assert_eq!(name, "wasm-thing");
    }

    #[test]
    fn find_manifest_errors_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_manifest_in(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("No SKILL.md or Skill.toml"), "got: {err}");
    }

    #[test]
    fn install_local_no_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bad-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("README.md"), "not a skill").unwrap();

        let target = tmp.path().join("installed");
        let result = install_skill(source.to_str().unwrap(), &target);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SKILL.md"));
    }

    #[test]
    fn install_local_already_exists() {
        let tmp = tempfile::tempdir().unwrap();

        let source = tmp.path().join("my-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), sample_skill_md()).unwrap();

        let target = tmp.path().join("installed");
        // Install once
        install_skill(source.to_str().unwrap(), &target).unwrap();
        // Install again — should fail
        let result = install_skill(source.to_str().unwrap(), &target);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn uninstall_skill_by_name() {
        let tmp = tempfile::tempdir().unwrap();

        let source = tmp.path().join("my-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), sample_skill_md()).unwrap();

        let target = tmp.path().join("installed");
        install_skill(source.to_str().unwrap(), &target).unwrap();

        // Uninstall by the name in SKILL.md
        let result = uninstall_skill("test-skill", &target);
        assert!(result.is_ok());

        // Verify it's gone
        assert!(!target.join("my-skill").exists());
    }

    #[test]
    fn removal_takes_the_whole_directory_and_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("hello-world");
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(dir.join("SKILL.md"), sample_skill_md()).unwrap();
        fs::write(dir.join(ACCESS_FILE), "{}").unwrap();
        fs::write(dir.join("docs").join("notes.md"), "notes").unwrap();

        remove_skill_dir("hello-world", &dir).unwrap();

        assert!(entries(tmp.path()).is_empty(), "{:?}", entries(tmp.path()));
        assert!(remove_skill_dir("hello-world", &dir).is_err());
    }

    #[test]
    fn uninstall_nonexistent_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("skills");
        fs::create_dir_all(&target).unwrap();

        let result = uninstall_skill("nonexistent", &target);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn uninstall_never_leaves_the_skills_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("skills");
        fs::create_dir_all(&target).unwrap();
        let outside = skill_source(tmp.path(), "outside", "1.0.0");

        for name in ["../outside", "..", "."] {
            assert!(find_skill_dir_by_name(name, &target).is_err(), "{name}");
            assert!(uninstall_skill(name, &target).is_err(), "{name}");
        }
        assert!(outside.join("SKILL.md").is_file());
        assert!(target.is_dir());
    }

    #[test]
    fn directory_names_are_single_components() {
        for name in ["hello-world", "Hello World", "v1.0", ".hidden"] {
            assert!(is_directory_name(name), "{name}");
        }
        for name in ["", ".", "..", "a/b", "../x", "/abs", "a\\b", "a/"] {
            assert!(!is_directory_name(name), "{name}");
        }
    }

    #[test]
    fn copy_dir_recursive_works() {
        let tmp = tempfile::tempdir().unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("file.txt"), "root file").unwrap();
        fs::write(src.join("subdir").join("nested.txt"), "nested file").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_recursive(&src, &dst).unwrap();

        assert!(dst.join("file.txt").is_file());
        assert!(dst.join("subdir").join("nested.txt").is_file());
        assert_eq!(
            fs::read_to_string(dst.join("file.txt")).unwrap(),
            "root file"
        );
        assert_eq!(
            fs::read_to_string(dst.join("subdir").join("nested.txt")).unwrap(),
            "nested file"
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo (its path ends in `.git`, so it installs as a git source) with
    /// two commits of `test-skill`: 1.0.0, then 2.0.0 on `main`. Returns the
    /// first commit's id.
    fn two_commit_skill_repo(repo: &Path) -> String {
        fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("SKILL.md"), sample_skill_md()).unwrap();
        git(repo, &["add", "SKILL.md"]);
        git(repo, &["commit", "-q", "-m", "1.0.0"]);
        let first = git(repo, &["rev-parse", "HEAD"]);
        fs::write(
            repo.join("SKILL.md"),
            sample_skill_md().replace("version: 1.0.0", "version: 2.0.0"),
        )
        .unwrap();
        git(repo, &["commit", "-q", "-am", "2.0.0"]);
        first
    }

    #[test]
    fn commit_sha_detection() {
        assert!(is_commit_sha("ad8309b7e6e0f0b61486e0eeadb8d722e5cfb9b7"));
        assert!(!is_commit_sha("main"));
        assert!(!is_commit_sha("v1.0.0"));
        assert!(!is_commit_sha("ad8309b7"));
        assert!(!is_commit_sha("zd8309b7e6e0f0b61486e0eeadb8d722e5cfb9b7"));
    }

    #[test]
    fn install_git_pinned_to_commit_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        let first = two_commit_skill_repo(&repo);

        let target = tmp.path().join("installed");
        let dest = install_skill_with(
            repo.to_str().unwrap(),
            &target,
            &InstallOptions {
                token: None,
                git_ref: Some(&first),
            },
        )
        .unwrap();

        let installed = fs::read_to_string(dest.join("SKILL.md")).unwrap();
        assert!(installed.contains("version: 1.0.0"), "got: {installed}");
    }

    #[test]
    fn install_git_branch_ref_takes_its_tip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        two_commit_skill_repo(&repo);

        let target = tmp.path().join("installed");
        let dest = install_skill_with(
            repo.to_str().unwrap(),
            &target,
            &InstallOptions {
                token: None,
                git_ref: Some("main"),
            },
        )
        .unwrap();

        let installed = fs::read_to_string(dest.join("SKILL.md")).unwrap();
        assert!(installed.contains("version: 2.0.0"), "got: {installed}");
    }

    #[test]
    fn install_git_unknown_commit_sha_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        two_commit_skill_repo(&repo);

        let target = tmp.path().join("installed");
        let err = install_skill_with(
            repo.to_str().unwrap(),
            &target,
            &InstallOptions {
                token: None,
                git_ref: Some("0000000000000000000000000000000000000001"),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pinned commit"), "got: {err}");
        assert!(!target.join("test-skill").exists());
    }

    #[test]
    fn find_skill_dir_by_metadata_name() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path();

        // Directory name differs from skill name in SKILL.md
        let dir = skills.join("my-custom-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), sample_skill_md()).unwrap();

        let found = find_skill_dir_by_name("test-skill", skills).unwrap();
        assert_eq!(found, dir);
    }

    fn skill_source(parent: &Path, dir_name: &str, version: &str) -> PathBuf {
        let source = parent.join(dir_name);
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            sample_skill_md().replace("version: 1.0.0", &format!("version: {version}")),
        )
        .unwrap();
        source
    }

    fn place_from(
        source: &Path,
        target: &Path,
        placement: Placement,
        access: Option<&str>,
    ) -> eyre::Result<Installed> {
        locate_skill(source.to_str().unwrap(), &InstallOptions::default())
            .and_then(|located| place_skill(&located, target, placement, access))
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn place_replace_swaps_in_the_new_version_and_keeps_config() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("installed");
        let v1 = skill_source(&tmp.path().join("v1"), "test-skill", "1.0.0");
        let v2 = skill_source(&tmp.path().join("v2"), "test-skill", "2.0.0");

        let first = place_from(&v1, &target, Placement::Fresh, None).unwrap();
        assert!(!first.replaced);
        assert_eq!(first.previous_version, None);
        fs::write(first.path.join("config.toml"), "greeting = \"hi\"\n").unwrap();

        let second = place_from(&v2, &target, Placement::Replace, None).unwrap();
        assert_eq!(second.path, first.path);
        assert!(second.replaced);
        assert_eq!(second.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(installed_version(&second.path).as_deref(), Some("2.0.0"));
        assert_eq!(
            fs::read_to_string(second.path.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );
        assert_eq!(entries(&target), ["test-skill"]);
    }

    /// A git source names its own directory from its manifest; one that
    /// climbs out of the skills directory would swap in for whatever it names.
    #[test]
    fn place_refuses_a_directory_name_that_leaves_the_skills_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("skills/installed");
        let source = skill_source(&tmp.path().join("src"), "test-skill", "1.0.0");
        let mut located =
            locate_skill(source.to_str().unwrap(), &InstallOptions::default()).unwrap();
        for escape in ["../../agents", "..", "a/b"] {
            located.dir_name = escape.to_string();
            let err = place_skill(&located, &target, Placement::Replace, None).unwrap_err();
            assert!(
                err.to_string().contains("not a directory name"),
                "{escape}: {err}"
            );
        }
        assert!(!tmp.path().join("agents").exists());
    }

    #[test]
    fn place_fresh_refuses_a_skill_installed_under_another_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("installed");
        skill_source(&target, "other-dir", "1.0.0");
        let source = skill_source(tmp.path(), "test-skill", "2.0.0");

        let err = place_from(&source, &target, Placement::Fresh, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already installed in"), "got: {err}");
        assert!(err.contains("other-dir"), "got: {err}");
        assert_eq!(entries(&target), ["other-dir"]);
    }

    #[test]
    fn place_replace_refuses_a_directory_holding_another_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("installed");
        let other = target.join("test-skill");
        fs::create_dir_all(&other).unwrap();
        fs::write(
            other.join("SKILL.md"),
            sample_skill_md().replace("name: test-skill", "name: another-skill"),
        )
        .unwrap();
        let source = skill_source(tmp.path(), "test-skill", "2.0.0");

        let err = place_from(&source, &target, Placement::Replace, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("holds skill 'another-skill'"), "got: {err}");
        assert_eq!(manifest_name(&other).as_deref(), Some("another-skill"));
        assert_eq!(entries(&target), ["test-skill"]);
    }

    #[test]
    fn locate_skill_reports_the_manifest_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        two_commit_skill_repo(&repo);
        let before = entries(tmp.path());

        let located = locate_skill(
            repo.to_str().unwrap(),
            &InstallOptions {
                token: None,
                git_ref: Some("main"),
            },
        )
        .unwrap();
        assert_eq!(located.name(), "test-skill");
        assert_eq!(located.kind(), ManifestKind::Markdown);
        assert_eq!(entries(tmp.path()), before);
    }

    #[test]
    fn located_git_skill_records_the_commit_it_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        let first = two_commit_skill_repo(&repo);
        let locate = |git_ref: &str| {
            locate_skill(
                repo.to_str().unwrap(),
                &InstallOptions {
                    token: None,
                    git_ref: Some(git_ref),
                },
            )
            .unwrap()
        };

        let tip = git(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(locate("main").commit(), Some(tip.as_str()));
        assert_eq!(locate(&first).commit(), Some(first.as_str()));
        let local = skill_source(tmp.path(), "local-skill", "1.0.0");
        let located = locate_skill(local.to_str().unwrap(), &InstallOptions::default()).unwrap();
        assert_eq!(located.commit(), None);
    }

    #[test]
    fn a_failed_clone_is_a_typed_error_with_the_same_message() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.git");
        let err = locate_skill(missing.to_str().unwrap(), &InstallOptions::default())
            .err()
            .expect("a missing repository cannot be cloned");
        assert!(err.downcast_ref::<CloneFailed>().is_some(), "got: {err:?}");
        assert!(
            err.to_string().starts_with("git clone failed: "),
            "got: {err}"
        );
    }

    #[test]
    fn placing_a_checkout_leaves_git_metadata_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("skills.git");
        two_commit_skill_repo(&repo);
        let target = tmp.path().join("installed");

        let from_git = place_from(&repo, &target, Placement::Fresh, None).unwrap();
        assert!(from_git.path.join("SKILL.md").is_file());
        assert!(!from_git.path.join(".git").exists());

        let local = skill_source(tmp.path(), "local-skill", "1.0.0");
        fs::create_dir_all(local.join(".git")).unwrap();
        fs::write(local.join(".git").join("config"), "[remote]").unwrap();
        fs::create_dir_all(local.join("vendor")).unwrap();
        fs::write(
            local.join("vendor").join(".git"),
            "gitdir: ../.git/modules/vendor",
        )
        .unwrap();
        let from_dir =
            place_from(&local, &tmp.path().join("other"), Placement::Fresh, None).unwrap();
        assert!(from_dir.path.join("SKILL.md").is_file());
        assert!(!from_dir.path.join(".git").exists());
        assert!(from_dir.path.join("vendor").is_dir());
        assert!(!from_dir.path.join("vendor").join(".git").exists());
    }

    #[test]
    fn a_copy_never_takes_an_access_record_from_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source = skill_source(tmp.path(), "test-skill", "1.0.0");
        fs::write(
            source.join(ACCESS_FILE.to_uppercase()),
            r#"{"owner":"mallory"}"#,
        )
        .unwrap();
        fs::write(source.join(".env.example"), "TOKEN=").unwrap();
        fs::create_dir_all(source.join("docs")).unwrap();
        fs::write(source.join("docs").join(ACCESS_FILE), "{}").unwrap();
        fs::write(source.join("docs").join(".notes"), "kept").unwrap();

        let placed = place_from(
            &source,
            &tmp.path().join("installed"),
            Placement::Fresh,
            None,
        )
        .unwrap();
        assert!(!placed.path.join(ACCESS_FILE).exists());
        assert!(!placed.path.join("docs").join(ACCESS_FILE).exists());
        assert!(placed.path.join(".env.example").is_file());
        assert!(placed.path.join("docs").join(".notes").is_file());
    }

    #[test]
    fn a_fresh_placement_writes_the_access_record_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let source = skill_source(tmp.path(), "test-skill", "1.0.0");
        fs::write(
            source.join(ACCESS_FILE.to_uppercase()),
            r#"{"owner":"mallory"}"#,
        )
        .unwrap();
        let record = r#"{"owner":"alice","visibility":"private","shares":[]}"#;

        let placed = place_from(
            &source,
            &tmp.path().join("installed"),
            Placement::Fresh,
            Some(record),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(placed.path.join(ACCESS_FILE)).unwrap(),
            record
        );
        assert_eq!(
            fs::read_to_string(source.join(ACCESS_FILE.to_uppercase())).unwrap(),
            r#"{"owner":"mallory"}"#
        );
    }

    #[test]
    fn a_replace_writes_only_the_record_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("installed");
        let v1 = skill_source(&tmp.path().join("v1"), "test-skill", "1.0.0");
        let v2 = skill_source(&tmp.path().join("v2"), "test-skill", "2.0.0");
        let first =
            place_from(&v1, &target, Placement::Fresh, Some(r#"{"owner":"alice"}"#)).unwrap();
        fs::write(first.path.join("config.toml"), "greeting = \"hi\"\n").unwrap();

        let second =
            place_from(&v2, &target, Placement::Replace, Some(r#"{"owner":"bob"}"#)).unwrap();
        assert_eq!(
            fs::read_to_string(second.path.join(ACCESS_FILE)).unwrap(),
            r#"{"owner":"bob"}"#
        );
        assert_eq!(
            fs::read_to_string(second.path.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );

        let third = place_from(&v1, &target, Placement::Replace, None).unwrap();
        assert!(!third.path.join(ACCESS_FILE).exists());
        assert!(third.path.join("config.toml").is_file());
    }

    #[test]
    fn installed_version_reads_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let valid = skill_source(tmp.path(), "valid", "1.2.3");
        let malformed = tmp.path().join("malformed");
        fs::create_dir_all(&malformed).unwrap();
        fs::write(malformed.join("SKILL.md"), "no frontmatter").unwrap();

        assert_eq!(installed_version(&valid).as_deref(), Some("1.2.3"));
        assert_eq!(installed_version(&malformed), None);
        assert_eq!(installed_version(&tmp.path().join("missing")), None);
    }
}
