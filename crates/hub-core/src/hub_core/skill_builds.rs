//! What building and installing a skill needs: its isolation, source and capabilities.

use crate::authz::Capability;
use workforce_skills::installer::GithubRef;

/// Whether a `cargo build` on this hub runs as the hub user or under the
/// privilege-separated runner. Set once at startup from `[exec]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildIsolation {
    /// Builds run as the hub uid: a skill's `build.rs` or a proc-macro can
    /// read everything the process can, including the vault key. Admin-only.
    InProcess,
    /// Builds dispatch to the runner daemon and run as the unprivileged agent
    /// user, which cannot read the vault. A member's build then reaches no
    /// further than `filesystem.*` already lets them reach.
    Sandboxed,
}

pub static BUILD_ISOLATION: std::sync::OnceLock<BuildIsolation> = std::sync::OnceLock::new();

pub fn build_isolation() -> BuildIsolation {
    BUILD_ISOLATION
        .get()
        .copied()
        .unwrap_or(BuildIsolation::InProcess)
}

/// Whether an install has to compile on the host, or only moves bytes that
/// were compiled elsewhere. The capability gate and the install tool both read
/// this, so they cannot disagree about which branch is about to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// Pre-built `.wasm` — a local file, an uploaded archive, or a
    /// digest-pinned GitHub release asset.
    Prebuilt,
    /// `cargo build` against a project directory or a cloned repo.
    HostCompile,
}

/// Classify an install by the source fields it names. Field precedence matches
/// the tool's own branch order (`wasm_path`, `project_path`, `github_repo`); a
/// GitHub install is prebuilt exactly when the release-asset strategy applies —
/// no subpath, no commit-sha ref — and a digest pins the bytes. An install
/// naming no source at all is `Prebuilt` because the tool rejects it outright;
/// nothing compiles.
pub fn install_source(
    wasm_path: Option<&str>,
    project_path: Option<&str>,
    github_repo: Option<&str>,
    sha256: Option<&str>,
) -> InstallSource {
    fn named(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|v| !v.is_empty())
    }
    match (named(wasm_path), named(project_path), named(github_repo)) {
        (Some(_), _, _) => InstallSource::Prebuilt,
        (None, Some(_), _) => InstallSource::HostCompile,
        (None, None, Some(repo)) => {
            match named(sha256).is_some() && !github_ref_builds_from_source(repo) {
                true => InstallSource::Prebuilt,
                false => InstallSource::HostCompile,
            }
        }
        (None, None, None) => InstallSource::Prebuilt,
    }
}

/// [`install_source`] over the raw tool input — what the capability gate sees.
pub fn install_source_kind(input: &serde_json::Value) -> InstallSource {
    let field = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
    install_source(
        field("wasm_path"),
        field("project_path"),
        field("github_repo"),
        field("sha256"),
    )
}

/// The capability `wasm_skill.build` needs in this deployment.
pub fn build_capability(isolation: BuildIsolation) -> Capability {
    match isolation {
        BuildIsolation::Sandboxed => Capability::SkillsAuthor,
        BuildIsolation::InProcess => Capability::ShellExec,
    }
}

/// The capability `wasm_skill.install` needs for this input: moving prebuilt
/// bytes is authoring, compiling is whatever compiling costs here.
pub fn install_capability(input: &serde_json::Value, isolation: BuildIsolation) -> Capability {
    match install_source_kind(input) {
        InstallSource::Prebuilt => Capability::SkillsAuthor,
        InstallSource::HostCompile => build_capability(isolation),
    }
}

/// A full 40-hex git commit id — the source pin for clone-and-build GitHub
/// installs, where the wasm digest is unknowable until the server builds it.
pub fn is_commit_sha(git_ref: &str) -> bool {
    workforce_skills::installer::is_commit_sha(git_ref)
}

/// Whether a `github_repo` ref can only be satisfied by cloning and compiling.
/// Mirrors the release-asset guard in [`fetch_from_github`] exactly — a subpath
/// or a commit-sha ref skips the asset strategy — so the capability gate and the
/// fetch agree on which branch will run.
pub fn github_ref_builds_from_source(repo_ref: &str) -> bool {
    let (_, tag, subpath) = parse_github_repo_ref(repo_ref);
    subpath.is_some() || tag.is_some_and(is_commit_sha)
}

/// Parse a `github_repo` argument into `(repo, tag, subpath)` with [`GithubRef`], the grammar
/// `system.install_external_skill` shares.
pub fn parse_github_repo_ref(repo_ref: &str) -> (&str, Option<&str>, Option<&str>) {
    let GithubRef {
        repo,
        git_ref,
        subpath,
    } = GithubRef::parse(repo_ref);
    (repo, git_ref, subpath)
}
