//! Shared utilities for coding tools.

use eyre::{eyre, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use workforce_agent::tools::exec_runner::{
    run_or_dispatch, EnvMode, ExecOutcome, ExecRequest, RunnerEndpoint,
};

use super::worktree::{is_repo_name, WorktreeManager};

/// Default hard timeout (seconds) for execution-class code tools that route a
/// command through the executor. The shared entry requires a timeout, but the
/// in-process predecessors (`code.build`/`code.cargo`/`code.lint`) ran with
/// none — this generous cap (matching the toolchain e2e budget) avoids killing
/// a legitimate large `cargo build`/`cargo audit`.
pub const CODE_TOOL_TIMEOUT_SECS: u64 = 600;

/// Spawn `program` with `args` in `workdir` through the privilege-separated
/// executor (when `runner` is set, fail-closed if unreachable) or
/// in-process (when `None`, the dev/local default). This is the single seam the
/// execution-class `code.*` tools use instead of `Command::new(...).output()`,
/// so they all share the one dispatch-or-local + fail-closed path (audit F7).
///
/// `allow_retry` is always `false`: the code tools never retried, and a build
/// command is not transparently re-run. Workdir confinement is the caller's
/// (each tool resolves the workspace via `resolve_repo_path`/`validate_path`
/// before calling), matching the prior bare `current_dir(workspace)` semantics.
pub async fn run_command(
    runner: Option<&RunnerEndpoint>,
    program: &str,
    args: &[&str],
    workdir: &Path,
    timeout_secs: u64,
) -> Result<ExecOutcome> {
    run_command_with_env(runner, program, args, workdir, &[], timeout_secs).await
}

/// [`run_command`] with `env` set over the environment the command inherits.
pub async fn run_command_with_env(
    runner: Option<&RunnerEndpoint>,
    program: &str,
    args: &[&str],
    workdir: &Path,
    env: &[(String, String)],
    timeout_secs: u64,
) -> Result<ExecOutcome> {
    let req = ExecRequest {
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        workdir: Some(workdir.display().to_string()),
        timeout_secs,
        allow_retry: false,
        env: env.to_vec(),
        stdin: None,
        scope: None,
        mcp: None,
    };
    run_or_dispatch(runner, req, &[], CODE_ENV).await
}

/// How an in-process `code.*` command inherits the hub's environment: as
/// `exec.run` does, without the secrets that decrypt the vault and forge
/// tokens. A build runs the repo's own `build.rs`, proc macros and scripts,
/// so it is the same exposure. Dispatched, the runner scrubs to its
/// allowlist instead.
pub(crate) const CODE_ENV: EnvMode = EnvMode::Inherit {
    remove: workforce_agent::tools::exec::EXEC_SECRET_VARS,
};

/// Global worktree manager, initialized once at startup.
/// Enables transparent worktree resolution in `resolve_repo_path`.
static WORKTREE_MANAGER: OnceLock<Arc<WorktreeManager>> = OnceLock::new();

/// Initialize the global worktree manager. Call once during hub startup.
pub fn init_worktree_manager(mgr: Arc<WorktreeManager>) {
    let _ = WORKTREE_MANAGER.set(mgr);
}

/// Where a relative `code.*` path starts: the session's active worktree when
/// there is one, so a workflow step editing a repo checkout sees the checkout,
/// and the workspace otherwise.
///
/// This base choice is the only thing `code.*` does differently from the other
/// path-taking tools; whether the result is *permitted* is decided by the
/// shared containment rule, never by the base.
fn session_base(allowed_paths: &[PathBuf]) -> Result<PathBuf> {
    let worktree_root = WORKTREE_MANAGER.get().and_then(|mgr| {
        workforce_agent::runner::CURRENT_SESSION_KEY
            .try_with(|k| k.clone())
            .ok()
            .and_then(|key| mgr.resolve_session_root(&key))
    });

    worktree_root
        .or_else(|| allowed_paths.first().cloned())
        .ok_or_else(|| eyre!("No workspace configured"))
}

/// Validate that a path is within the allowed directories.
///
/// This is a critical security function that prevents path traversal attacks.
/// Containment is decided — and the validated location returned — by the same
/// rule `filesystem.*` and `wasm_skill.*` use, so an agent-created symlink or
/// a `..` through directories that do not exist yet cannot resolve to one path
/// and be used as another.
pub fn validate_path(path: &Path, allowed_paths: &[PathBuf]) -> Result<PathBuf> {
    if allowed_paths.is_empty() {
        // No restrictions if empty
        return Ok(path.to_path_buf());
    }

    let resolved = match path.is_absolute() {
        true => path.to_path_buf(),
        false => session_base(allowed_paths)?.join(path),
    };

    workforce_agent::tools::paths::CanonicalRoots::new(allowed_paths)
        .resolve(&resolved)
        .ok_or_else(|| {
            eyre!(workforce_agent::tools::paths::refusal(
                &path.display().to_string(),
                &resolved
            ))
        })
}

/// What the `repo` parameter of every `code.*` tool tells the model. The
/// sentence names both spellings the resolver takes and where the other repos
/// are, because agents learn repo names from the `repo.*` catalog and bring
/// them here.
pub const REPO_PARAM: &str = "A repo checked out in this workspace: `owner/repo`, or the repo \
    name alone when only one checkout has it. A repo the hub has indexed but not checked out here \
    is read with repo.grep, repo.glob and repo.read instead.";

/// Resolve the `repo` argument of a `code.*` tool to its directory.
///
/// `repo` names a checkout under `workspace/repos/` — see
/// [`checked_out_repo`] for the spellings accepted. The session's worktree of
/// that checkout is returned when it has one; a non-workflow session
/// (DM/channel) is given one on first access, so its edits never meet another
/// session's. Errors when nothing is checked out under the name, when the name
/// is ambiguous, or when it resolves outside the workspace.
///
/// Reads create the worktree too, and must: `code.edit` takes a path, not a
/// repo, and resolves it against the session's worktree. A read that left the
/// session without one would send the edit that follows to the shared
/// checkout.
pub fn resolve_repo_path(repo: &str, workspace: &Path) -> Result<PathBuf> {
    let session = WORKTREE_MANAGER.get().and_then(|mgr| {
        let key = workforce_agent::runner::CURRENT_SESSION_KEY
            .try_with(|k| k.clone())
            .ok()?;
        Some((mgr, key))
    });
    let session = session.as_ref().map(|(mgr, key)| (*mgr, key.as_str()));
    resolve_repo_for(repo, workspace, session)
}

/// [`resolve_repo_path`] for a given session: its worktree manager and session
/// key, or `None` outside a session or before worktrees are set up.
fn resolve_repo_for(
    repo: &str,
    workspace: &Path,
    session: Option<(&Arc<WorktreeManager>, &str)>,
) -> Result<PathBuf> {
    let wanted = repo.trim().trim_matches('/');
    if !is_repo_name(wanted) {
        return Err(not_checked_out(repo, workspace));
    }
    // Worktrees are keyed by the checkout's own name, so the argument is
    // resolved to it first: `api` and `acme/api` must reach one worktree. A
    // workflow's worktree can stand for a repo this workspace does not hold,
    // so the name as given is still looked up before the miss is reported.
    let checkout = checked_out_repo(wanted, workspace);
    let name = match &checkout {
        Ok(Some(checkout)) => checkout.name.as_str(),
        _ => wanted,
    };
    let existing = session
        .and_then(|(mgr, key)| mgr.resolve_for_session(key, name))
        .filter(|path| path.exists());
    if let Some(existing) = existing {
        return Ok(existing);
    }

    let checkout = checkout?.ok_or_else(|| not_checked_out(repo, workspace))?;
    match session {
        Some((mgr, key)) if !key.starts_with("workflow:") && is_checkout(&checkout.path) => {
            Ok(ensure_session_worktree(mgr, key, &checkout.name, workspace)
                .unwrap_or(checkout.path))
        }
        _ => Ok(checkout.path),
    }
}

/// Create the session's worktree of a checkout. Bridges sync → async with
/// `block_in_place`, since git is spawned; `None` when it cannot be made, and
/// the caller falls back to the checkout.
fn ensure_session_worktree(
    mgr: &Arc<WorktreeManager>,
    key: &str,
    name: &str,
    workspace: &Path,
) -> Option<PathBuf> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    tokio::task::block_in_place(|| {
        handle.block_on(mgr.ensure_for_session(key, name, workspace, None))
    })
    .ok()
}

fn not_checked_out(repo: &str, workspace: &Path) -> eyre::Report {
    eyre!(
        "Repository '{repo}' is not checked out in this workspace. {} A repo the hub has \
         indexed but not checked out here is read with repo.grep, repo.glob and repo.read \
         under the same name.",
        checked_out_note(workspace)
    )
}

/// A checkout in this workspace: the name [`list_available_repos`] gives it and
/// its resolved directory.
#[derive(Debug)]
struct Checkout {
    name: String,
    path: PathBuf,
}

/// The checkout a `repo` argument names: the one spelled exactly so, else the
/// one whose full name or repo name matches it whatever its case, else the
/// directory at that path under `repos/`. `None` when nothing is there; an
/// error when several checkouts share the name, or when the directory resolves
/// outside the workspace (a symlink under `repos/` may point anywhere).
///
/// The listed spelling is what comes back, never the caller's: on a
/// filesystem that ignores case `Acme/API` would otherwise be a second name
/// for `acme/api`, and a second worktree.
fn checked_out_repo(wanted: &str, workspace: &Path) -> Result<Option<Checkout>> {
    let available = list_available_repos(workspace);
    let listed = match available.iter().find(|name| name.as_str() == wanted) {
        Some(exact) => Some(exact),
        None => match repos_named(wanted, &available).as_slice() {
            [] => None,
            [only] => Some(*only),
            several => {
                return Err(eyre!(
                    "Repository '{wanted}' is ambiguous in this workspace; pass one of: {}",
                    several
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        },
    };
    let name = listed.map_or(wanted, String::as_str);
    let Ok(path) = workspace.join("repos").join(name).canonicalize() else {
        return Ok(None);
    };
    if !path.is_dir() {
        return Ok(None);
    }
    if !path.starts_with(workspace.canonicalize()?) {
        return Err(eyre!(
            "Repository '{wanted}' resolves outside this workspace and cannot be used."
        ));
    }
    Ok(Some(Checkout {
        name: name.to_string(),
        path,
    }))
}

/// The checkouts in `available` that `name` could mean, ignoring ASCII case:
/// those spelled exactly so, else those whose repo name (the last segment) is
/// `name`. Agents and people alike know a repo by its name and leave the owner
/// off.
pub fn repos_named<'a>(name: &str, available: &'a [String]) -> Vec<&'a String> {
    let matching = |part: fn(&str) -> &str| -> Vec<&'a String> {
        available
            .iter()
            .filter(|full| part(full).eq_ignore_ascii_case(name))
            .collect()
    };
    match matching(|full| full) {
        exact if !exact.is_empty() => exact,
        _ => matching(|full| full.rsplit('/').next().unwrap_or(full)),
    }
}

/// The sentence a missing or unknown `repo` is answered with: the checkouts
/// that can be named.
pub fn checked_out_note(workspace: &Path) -> String {
    match list_available_repos(workspace).as_slice() {
        [] => "No repos are checked out in this workspace.".to_string(),
        available => format!("Checked-out repos: {}.", available.join(", ")),
    }
}

/// The repos checked out under `workspace/repos/`, as the `repo` parameter
/// names them: `owner/repo`, or the bare name of a checkout sitting directly
/// under `repos/`. A checkout is a directory holding `.git`, so the contents
/// of a directly placed checkout are never mistaken for repos.
pub fn list_available_repos(workspace: &Path) -> Vec<String> {
    let mut repos: Vec<String> = subdirectories(&workspace.join("repos"))
        .into_iter()
        .flat_map(|(name, path)| match is_checkout(&path) {
            true => vec![name],
            false => subdirectories(&path)
                .into_iter()
                .filter(|(_, repo)| is_checkout(repo))
                .map(|(repo, _)| format!("{name}/{repo}"))
                .collect(),
        })
        .collect();
    repos.sort();
    repos
}

fn is_checkout(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn subdirectories(dir: &Path) -> Vec<(String, PathBuf)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| Some((path.file_name()?.to_string_lossy().into_owned(), path)))
        .collect()
}

/// Detect project type based on marker files.
pub fn detect_project_type(root: &Path) -> Option<&'static str> {
    if root.join("Cargo.toml").exists() {
        Some("rust")
    } else if root.join("package.json").exists() {
        Some("node")
    } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
        Some("python")
    } else if root.join("Gemfile").exists() {
        Some("ruby")
    } else if root.join("go.mod").exists() {
        Some("go")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn code_commands_inherit_the_environment_without_the_hubs_secrets() {
        assert!(matches!(
            CODE_ENV,
            EnvMode::Inherit { remove } if remove == workforce_agent::tools::exec::EXEC_SECRET_VARS
        ));
    }

    #[tokio::test]
    async fn a_command_runs_with_the_environment_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let env = [("GATE_PROBE".to_string(), "warm".to_string())];
        let out = run_command_with_env(
            None,
            "sh",
            &["-c", "echo $GATE_PROBE"],
            tmp.path(),
            &env,
            10,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.trim(), "warm");
    }

    /// With no worktree for the session, a relative path starts at the
    /// workspace. The worktree-first branch needs the process-global manager
    /// (`WORKTREE_MANAGER`), so only this half is reachable from a unit test;
    /// what matters for containment is that the base never widens it.
    #[test]
    fn a_relative_path_falls_back_to_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let allowed = vec![workspace.clone()];
        assert_eq!(session_base(&allowed).unwrap(), workspace);
        assert_eq!(
            validate_path(Path::new("src/main.rs"), &allowed).unwrap(),
            workspace.join("src/main.rs")
        );
    }

    /// A worktree root is only a starting point: a path under the worktrees
    /// root passes because that root is allowed, not because it was the base.
    #[test]
    fn a_second_allowed_root_is_reachable_by_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let worktrees = workspace.join("worktrees");
        std::fs::create_dir_all(worktrees.join("wf-1/repo")).unwrap();
        let allowed = vec![workspace, worktrees.clone()];
        let inside = worktrees.join("wf-1/repo/lib.rs");
        assert_eq!(validate_path(&inside, &allowed).unwrap(), inside);
    }

    /// The hole the shared rule closes: canonicalisation fails for a path
    /// whose directories do not exist, and the un-normalised comparison used
    /// to admit it.
    #[test]
    fn traversal_through_missing_directories_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let allowed = vec![workspace.clone()];
        assert!(validate_path(Path::new("a/b/../../../etc/evil"), &allowed).is_err());
        assert!(validate_path(Path::new("a/b/../c/file.rs"), &allowed).is_ok());
    }

    /// An agent can create a symlink in its own workspace; `..` through it
    /// lands in the target's parent, not back in the workspace.
    #[test]
    #[cfg(unix)]
    fn traversal_through_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        std::fs::write(outside.join("secret.txt"), "classified").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        let allowed = vec![workspace];

        assert!(validate_path(Path::new("link/../secret.txt"), &allowed).is_err());
        assert!(validate_path(Path::new("link/secret.txt"), &allowed).is_err());
    }

    /// What is validated is what is returned, so a caller cannot write to a
    /// different file than the one that was checked.
    #[test]
    fn the_returned_path_is_the_validated_location() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(workspace.join("a")).unwrap();
        let allowed = vec![workspace.clone()];
        let validated = validate_path(Path::new("a/../b.rs"), &allowed).unwrap();
        assert_eq!(validated, workspace.join("b.rs"));
        std::fs::write(&validated, "usable").unwrap();
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let temp = TempDir::new().unwrap();
        let allowed = vec![temp.path().to_path_buf()];

        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let result = validate_path(&test_file, &allowed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let temp = TempDir::new().unwrap();
        let allowed = vec![temp.path().join("subdir")];

        let outside = temp.path().join("outside.txt");
        let result = validate_path(&outside, &allowed);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_path_relative() {
        let temp = TempDir::new().unwrap();
        let allowed = vec![temp.path().to_path_buf()];

        fs::write(temp.path().join("test.txt"), "test").unwrap();

        let result = validate_path(Path::new("test.txt"), &allowed);
        assert!(result.is_ok());
    }

    /// A workspace holding `acme/api` and `acme/web` under an owner, and
    /// `mono` checked out directly under `repos/` with source directories of
    /// its own.
    fn workspace_with_checkouts() -> TempDir {
        let temp = TempDir::new().unwrap();
        for checkout in ["acme/api", "acme/web", "mono"] {
            fs::create_dir_all(temp.path().join("repos").join(checkout).join(".git")).unwrap();
        }
        fs::create_dir_all(temp.path().join("repos/mono/src")).unwrap();
        fs::create_dir_all(temp.path().join("repos/acme/scratch")).unwrap();
        temp
    }

    fn resolved(repo: &str, workspace: &TempDir) -> PathBuf {
        resolve_repo_path(repo, workspace.path()).unwrap()
    }

    fn checkout(workspace: &TempDir, name: &str) -> PathBuf {
        workspace
            .path()
            .join("repos")
            .join(name)
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn only_checkouts_are_listed_whichever_level_they_sit_at() {
        let workspace = workspace_with_checkouts();
        assert_eq!(
            list_available_repos(workspace.path()),
            ["acme/api", "acme/web", "mono"]
        );
    }

    #[test]
    fn a_repo_is_found_by_its_name_alone_when_one_checkout_has_it() {
        let workspace = workspace_with_checkouts();
        assert_eq!(
            resolved("api", &workspace),
            checkout(&workspace, "acme/api")
        );
        assert_eq!(resolved("mono", &workspace), checkout(&workspace, "mono"));
    }

    #[test]
    fn a_repo_name_matches_whatever_case_it_is_written_in() {
        let workspace = workspace_with_checkouts();
        assert_eq!(
            resolved("API", &workspace),
            checkout(&workspace, "acme/api")
        );
        assert_eq!(
            resolved("Acme/Web", &workspace),
            checkout(&workspace, "acme/web")
        );
    }

    #[test]
    fn the_full_owner_and_repo_name_still_resolves() {
        let workspace = workspace_with_checkouts();
        assert_eq!(
            resolved("acme/api", &workspace),
            checkout(&workspace, "acme/api")
        );
    }

    #[test]
    fn a_name_two_checkouts_share_is_refused_with_both_named() {
        let workspace = workspace_with_checkouts();
        fs::create_dir_all(workspace.path().join("repos/other/api/.git")).unwrap();
        let error = resolve_repo_path("api", workspace.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("acme/api, other/api"), "{error}");
    }

    #[test]
    fn a_repo_that_is_not_checked_out_names_the_checkouts_and_the_repo_tools() {
        let workspace = workspace_with_checkouts();
        let error = resolve_repo_path("billing", workspace.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("'billing' is not checked out in this workspace"),
            "{error}"
        );
        assert!(
            error.contains("Checked-out repos: acme/api, acme/web, mono."),
            "{error}"
        );
        assert!(
            ["repo.grep", "repo.glob", "repo.read"]
                .iter()
                .all(|tool| error.contains(tool)),
            "{error}"
        );
    }

    #[test]
    fn an_empty_workspace_says_nothing_is_checked_out() {
        let workspace = TempDir::new().unwrap();
        let error = resolve_repo_path("api", workspace.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("No repos are checked out in this workspace."),
            "{error}"
        );
    }

    /// A workspace beside a repository that is not its own — another agent's
    /// checkout, as far as this one is concerned.
    fn workspace_beside_a_foreign_checkout() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("mine/workspace");
        let foreign = temp.path().join("theirs/workspace/repos/acme/secret");
        fs::create_dir_all(workspace.join("repos/acme/api/.git")).unwrap();
        fs::create_dir_all(foreign.join(".git")).unwrap();
        (temp, workspace, foreign)
    }

    #[test]
    fn a_repo_argument_that_is_not_a_name_is_refused_as_not_checked_out() {
        let (_temp, workspace, foreign) = workspace_beside_a_foreign_checkout();
        let absolute = foreign.to_string_lossy().into_owned();
        for argument in [
            absolute.as_str(),
            "../../theirs/workspace/repos/acme/secret",
            "acme/../../../theirs/workspace/repos/acme/secret",
            "..",
            ".",
            "",
            "  ",
            "acme//api",
            "acme\\api",
        ] {
            let error = resolve_repo_path(argument, &workspace)
                .expect_err(argument)
                .to_string();
            assert!(
                error.contains("is not checked out"),
                "{argument:?}: {error}"
            );
            assert!(error.contains("Checked-out repos: acme/api."), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_under_repos_cannot_lead_out_of_the_workspace() {
        let (_temp, workspace, foreign) = workspace_beside_a_foreign_checkout();
        std::os::unix::fs::symlink(&foreign, workspace.join("repos/acme/linked")).unwrap();

        for argument in ["acme/linked", "linked"] {
            let error = resolve_repo_path(argument, &workspace)
                .expect_err(argument)
                .to_string();
            assert!(error.contains("outside this workspace"), "{error}");
        }
    }

    /// Only a filesystem that tells case apart can hold both; on one that
    /// does not, the second checkout is the first and there is nothing to
    /// refuse.
    #[test]
    fn two_checkouts_differing_only_by_case_are_never_guessed_between() {
        let workspace = TempDir::new().unwrap();
        for checkout in ["acme/api", "Acme/API"] {
            fs::create_dir_all(workspace.path().join("repos").join(checkout).join(".git")).unwrap();
        }
        if list_available_repos(workspace.path()).len() != 2 {
            return;
        }

        for exact in ["acme/api", "Acme/API"] {
            assert_eq!(resolved(exact, &workspace), checkout(&workspace, exact));
        }
        for vague in ["api", "ACME/api"] {
            let error = resolve_repo_path(vague, workspace.path())
                .unwrap_err()
                .to_string();
            assert!(error.contains("ambiguous"), "{error}");
            assert!(error.contains("Acme/API, acme/api"), "{error}");
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let done = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(done.status.success(), "git {args:?}: {done:?}");
    }

    /// A workspace whose `acme/api` is a real repository with one commit, and
    /// a worktree manager of its own; `None` where git is not installed.
    fn workspace_with_a_repository() -> Option<(TempDir, PathBuf, Arc<WorktreeManager>)> {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()?;
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        let repo = workspace.join("repos/acme/api");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("README.md"), "# api").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        let manager = Arc::new(WorktreeManager::new(&temp.path().join("data")).unwrap());
        Some((temp, workspace, manager))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_spelling_of_a_repo_reaches_the_sessions_one_worktree() {
        let Some((_temp, workspace, manager)) = workspace_with_a_repository() else {
            return;
        };
        let session = Some((&manager, "slack:ws:C1:T1"));

        let worktree = resolve_repo_for("acme/api", &workspace, session).unwrap();
        assert!(!worktree.starts_with(workspace.canonicalize().unwrap()));
        for spelling in ["api", "API", "Acme/Api", "/acme/api/"] {
            assert_eq!(
                resolve_repo_for(spelling, &workspace, session).unwrap(),
                worktree,
                "{spelling}"
            );
        }
        assert_eq!(manager.list_worktrees().await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_session_gets_a_worktree_of_its_own() {
        let Some((_temp, workspace, manager)) = workspace_with_a_repository() else {
            return;
        };
        let one = resolve_repo_for("api", &workspace, Some((&manager, "slack:ws:C1:T1"))).unwrap();
        let other =
            resolve_repo_for("api", &workspace, Some((&manager, "slack:ws:C1:T2"))).unwrap();
        assert_ne!(one, other);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_is_never_given_a_worktree_of_a_repository_outside_its_workspace() {
        let Some((temp, workspace, manager)) = workspace_with_a_repository() else {
            return;
        };
        let foreign = temp.path().join("theirs/workspace/repos/acme/api");
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::rename(workspace.join("repos/acme/api"), &foreign).unwrap();
        let absolute = foreign.to_string_lossy().into_owned();
        let session = Some((&manager, "slack:ws:C1:T1"));

        for argument in [
            absolute.as_str(),
            "../../../theirs/workspace/repos/acme/api",
        ] {
            assert!(
                resolve_repo_for(argument, &workspace, session).is_err(),
                "{argument}"
            );
        }
        assert!(manager.list_worktrees().await.is_empty());
        assert!(!foreign.with_extension("git").exists());
    }

    #[test]
    fn test_detect_rust_project() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();

        assert_eq!(detect_project_type(temp.path()), Some("rust"));
    }

    #[test]
    fn test_detect_node_project() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        assert_eq!(detect_project_type(temp.path()), Some("node"));
    }
}
