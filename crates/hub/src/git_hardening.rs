//! The one way the hub spawns `git`, and the lock on a repository's config.
//!
//! Under privilege separation the hub (uid `workforce`, holding the vault key)
//! and the agent (uid `agent`) share repositories on purpose: the agent has to
//! edit a worktree and commit, so it can write that repository's `.git`. Git
//! treats repo-local config as executable surface — `core.fsmonitor`,
//! `core.hooksPath`, `diff.external` and filter drivers all name commands git
//! runs — and the image sets `safe.directory=*` system-wide, so hub-side git
//! works in those repositories without complaint. A plain `git` run by the hub
//! inside a tree the agent can write is therefore a way to execute code as the
//! hub, with the vault key in its environment.
//!
//! The guard lives here rather than at each call site because deciding per-site
//! which repositories an agent can reach is what went wrong: the worktree
//! manager reasoned about it correctly and every other site did not.
//! `hub_git_is_always_hardened` fails the suite if a new one appears.

use std::path::Path;

use tokio::process::Command;

/// Restated from the image's `/etc/gitconfig`, which `GIT_CONFIG_NOSYSTEM`
/// drops. Inheriting these made a dependency invisible until it broke: without
/// an identity `cc_rebase` fails on "committer identity unknown", and without
/// `safe.directory` every command in an agent-owned checkout fails on "dubious
/// ownership".
const RESTATED_SYSTEM_CONFIG: [(&str, &str); 3] = [
    ("user.name", "Workforce Agent"),
    ("user.email", "agent@workforce.local"),
    ("safe.directory", "*"),
];

/// The two keys that name a command git runs during ordinary read-only
/// operations, so a planted one needs no attacker interaction at all.
const DISABLED_EXEC_KEYS: [(&str, &str); 2] =
    [("core.hooksPath", "/dev/null"), ("core.fsmonitor", "false")];

/// A `git` that ignores every configuration file it did not receive on its own
/// command line, except the repository-local config — which the hub itself
/// wrote the remotes and refspecs into, and so still needs.
pub fn hardened_git() -> Command {
    let mut cmd = Command::new("git");
    cmd.envs(HARDENED_GIT_ENV).args(hardened_git_args());
    cmd
}

/// The environment half of the hardening: no system or global configuration.
pub const HARDENED_GIT_ENV: [(&str, &str); 2] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
];

/// The argument half: the restated system settings and the disabled exec
/// keys, ahead of whatever the caller runs.
pub fn hardened_git_args() -> Vec<String> {
    RESTATED_SYSTEM_CONFIG
        .iter()
        .chain(&DISABLED_EXEC_KEYS)
        .flat_map(|(key, value)| ["-c".to_string(), format!("{key}={value}")])
        .collect()
}

/// Take group and world write off a repository's config files.
///
/// Not a boundary, and must not be relied on as one: git rewrites config by
/// creating `config.lock` and renaming it over the target, which needs write on
/// the *directory*, not the file. Every tree the agent works in has a
/// group-writable directory by construction — that is what lets it commit — so
/// an agent that targets the config directly still replaces it. Verified
/// against a live install: an agent-uid `git config --local` succeeds at 0640.
///
/// What it buys is the removal of world-read from a file that holds a PAT, and
/// the accidental case. [`hardened_git`] is the guard.
pub fn lock_down_config(git_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["config", "config.worktree"] {
            let path = git_dir.join(name);
            if path.exists() {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640));
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = git_dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn git_in(repo: &Path, args: &[&str]) {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .await
            .unwrap();
    }

    async fn init_repo(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q"]).await;
        repo
    }

    /// A repo whose local config runs a marker-dropping script through `key`.
    /// Returns the repo and the marker path.
    async fn repo_with_planted_exec(tmp: &TempDir, key: &str) -> (PathBuf, PathBuf) {
        let repo = init_repo(tmp.path()).await;
        let marker = tmp.path().join("pwned");
        let script = tmp.path().join("payload.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let value = if key == "core.hooksPath" {
            let hooks = tmp.path().join("hooks");
            fs::create_dir_all(&hooks).unwrap();
            fs::copy(&script, hooks.join("post-index-change")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    hooks.join("post-index-change"),
                    fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            hooks.display().to_string()
        } else {
            script.display().to_string()
        };
        git_in(&repo, &["config", "--local", key, &value]).await;
        (repo, marker)
    }

    #[tokio::test]
    async fn planted_fsmonitor_runs_unhardened_and_not_under_hardened_git() {
        if !git_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (repo, marker) = repo_with_planted_exec(&tmp, "core.fsmonitor").await;

        let _ = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()
            .await;
        assert!(
            marker.exists(),
            "vacuous: a plain git did not run the planted command, so the \
             hardened assertion below proves nothing"
        );
        fs::remove_file(&marker).unwrap();

        let _ = hardened_git()
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()
            .await;
        assert!(!marker.exists(), "hardened git ran the planted fsmonitor");
    }

    #[tokio::test]
    async fn planted_hooks_path_is_not_honoured_under_hardened_git() {
        if !git_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (repo, marker) = repo_with_planted_exec(&tmp, "core.hooksPath").await;

        let _ = hardened_git()
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()
            .await;
        assert!(
            !marker.exists(),
            "hardened git honoured the planted hooksPath"
        );
    }

    #[tokio::test]
    async fn hardened_git_can_commit_without_ambient_config() {
        if !git_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path()).await;
        fs::write(repo.join("a.txt"), "hello").unwrap();

        let add = hardened_git()
            .args(["add", "a.txt"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert!(add.status.success(), "add: {:?}", add);

        let commit = hardened_git()
            .args(["commit", "-m", "init"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert!(
            commit.status.success(),
            "hardened git cannot commit — GIT_CONFIG_NOSYSTEM dropped the \
             identity the image provides: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    #[tokio::test]
    async fn hardened_git_still_reads_repo_local_config() {
        if !git_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path()).await;
        git_in(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        )
        .await;

        let out = hardened_git()
            .args(["config", "--get", "remote.origin.url"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "https://github.com/acme/widgets.git",
            "hardening must not drop the remotes the hub itself configured"
        );
    }

    #[test]
    fn lock_down_config_takes_group_and_world_write_off() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = TempDir::new().unwrap();
            for name in ["config", "config.worktree"] {
                let path = tmp.path().join(name);
                fs::write(&path, "[core]\n").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
            }

            lock_down_config(tmp.path());

            for name in ["config", "config.worktree"] {
                let mode = fs::metadata(tmp.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o640, "{name} left writable by the agent group");
            }
        }
    }

    #[test]
    fn lock_down_config_ignores_a_repository_without_config() {
        lock_down_config(TempDir::new().unwrap().path());
    }

    /// Where a file's `#[cfg(test)] mod tests` (or `pub(crate) mod tests`) begins, or its length.
    ///
    /// Anchoring on the test module rather than the first `#[cfg(test)]` is
    /// load-bearing: files carrying test-only helpers above their module (16 of
    /// them in `gateway.rs` alone) would otherwise hide every production line
    /// after the first marker from this scan.
    fn test_module_start(lines: &[&str]) -> usize {
        lines
            .iter()
            .enumerate()
            .find(|(i, line)| {
                line.trim() == "#[cfg(test)]"
                    && lines[i + 1..]
                        .iter()
                        .find(|l| !l.trim().is_empty())
                        .is_some_and(|l| {
                            let l = l.trim_start();
                            l.strip_prefix("pub(crate) ")
                                .unwrap_or(l)
                                .starts_with("mod tests")
                        })
            })
            .map_or(lines.len(), |(i, _)| i)
    }

    /// The design's claim, asserted over the source tree: an unhardened
    /// hub-side `git` is the bug this module exists to prevent, and a new one
    /// arrives by copy-paste rather than by argument.
    #[test]
    fn hub_git_is_always_hardened() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root.clone()];

        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs")
                    || path.file_name().and_then(|f| f.to_str()) == Some("git_hardening.rs")
                {
                    continue;
                }
                let src = fs::read_to_string(&path).unwrap();
                let lines: Vec<&str> = src.lines().collect();
                let lines = &lines[..test_module_start(&lines)];
                lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.contains(r#"Command::new("git")"#))
                    .for_each(|(n, _)| {
                        offenders.push(format!(
                            "{}:{}",
                            path.strip_prefix(&root).unwrap_or(&path).display(),
                            n + 1
                        ));
                    });
            }
        }

        assert!(
            offenders.is_empty(),
            "hub-side git must go through git_hardening::hardened_git(); found \
             plain Command::new(\"git\") at: {}",
            offenders.join(", ")
        );
    }
}
