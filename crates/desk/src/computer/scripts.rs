//! The project's own scripts, read for a boss acting fully before it allows
//! what ships through one ([`super::boss::Script`]): `package.json`'s
//! scripts, a Makefile's recipes, a shell script's lines. Only a file the
//! repository has on `origin`'s default branch, unchanged since, is read;
//! anything else, and anything these readers can't follow (make's
//! variables, `include`, a shell script's `if` or functions), is the
//! person's.
//!
//! The readers ([`package_scripts`], [`make_recipe`], [`shell_lines`]) are
//! pure; [`Reader`] adds the disk and git.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::boss::Script;

type Read = Result<Vec<String>, String>;

/// `npm run name`'s commands from `package.json` `body`: its `pre`, own and
/// `post` scripts, in that order; `None`: `npm version`'s lifecycle
/// (`preversion`, `version`, `postversion`), none of which need be there.
pub fn package_scripts(body: &str, name: Option<&str>) -> Read {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "Desk can't read its package.json".to_string())?;
    let scripts = json.get("scripts").and_then(|s| s.as_object());
    let get = |k: &str| -> Result<Option<String>, String> {
        match scripts.and_then(|s| s.get(k)) {
            None => Ok(None),
            Some(serde_json::Value::String(v)) => Ok(Some(v.clone())),
            Some(_) => Err("Desk can't read its package.json's scripts".into()),
        }
    };
    let (base, must) = match name {
        Some(n) => (n.to_string(), true),
        None => ("version".to_string(), false),
    };
    let own = get(&base)?;
    if must && own.is_none() {
        return Err(format!("its package.json has no {base} script"));
    }
    Ok([
        get(&format!("pre{base}"))?,
        own,
        get(&format!("post{base}"))?,
    ]
    .into_iter()
    .flatten()
    .collect())
}

/// `make target`'s commands from Makefile `body`: its prerequisites'
/// recipes first (each once), then its own. Variables, `include`, pattern
/// and double-colon rules, and a target with no rule are the person's.
pub fn make_recipe(body: &str, target: &str) -> Read {
    let mut joined: Vec<String> = Vec::new();
    for line in body.lines() {
        match joined.last_mut() {
            Some(prev) if prev.ends_with('\\') => {
                prev.pop();
                prev.truncate(prev.trim_end().len());
                prev.push(' ');
                prev.push_str(line.trim_start());
            }
            _ => joined.push(line.to_string()),
        }
    }
    if joined.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("include ") || t.starts_with("-include ") || t.starts_with("sinclude ")
    }) {
        return Err("its Makefile includes another Desk doesn't follow".into());
    }
    // Each rule: its targets, prerequisites and recipe lines.
    let mut rules: Vec<(Vec<String>, Vec<String>, Vec<String>)> = Vec::new();
    for line in &joined {
        if let Some(cmd) = line.strip_prefix('\t') {
            if let Some(rule) = rules.last_mut() {
                rule.2.push(cmd.to_string());
            }
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((targets, deps)) = t.split_once(':') {
            if targets.contains('=') || deps.starts_with('=') {
                // `A := b`: a variable.
                continue;
            }
            if deps.starts_with(':') || targets.contains('%') {
                rules.push((Vec::new(), Vec::new(), Vec::new()));
                continue;
            }
            let deps = deps.split(';').next().unwrap_or_default();
            rules.push((
                targets.split_whitespace().map(str::to_string).collect(),
                deps.split_whitespace().map(str::to_string).collect(),
                Vec::new(),
            ));
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        rules: &[(Vec<String>, Vec<String>, Vec<String>)],
        target: &str,
        seen: &mut HashSet<String>,
        out: &mut Vec<String>,
        top: bool,
    ) -> Result<(), String> {
        if !seen.insert(target.to_string()) {
            return Ok(());
        }
        let Some(rule) = rules.iter().find(|r| r.0.iter().any(|t| t == target)) else {
            return match top {
                true => Err(format!("its Makefile has no {target} target")),
                // A file it depends on, with no rule of its own.
                false => Ok(()),
            };
        };
        for dep in &rule.1 {
            walk(rules, dep, seen, out, false)?;
        }
        for cmd in &rule.2 {
            let cmd = cmd.trim().trim_start_matches(['@', '-', '+']).trim();
            if cmd.is_empty() || cmd.starts_with('#') {
                continue;
            }
            if cmd.contains('$') {
                return Err("its Makefile's recipe uses variables Desk doesn't read".into());
            }
            out.push(cmd.to_string());
        }
        Ok(())
    }
    walk(&rules, target, &mut seen, &mut out, true)?;
    Ok(out)
}

/// A shell script's commands from `body`: its lines, comments and `set`
/// options left out, continued lines joined. Its own control flow (`if`,
/// loops, functions, `case`) is the person's.
pub fn shell_lines(body: &str) -> Read {
    let mut out: Vec<String> = Vec::new();
    let mut open = false;
    for line in body.lines() {
        let t = line.trim();
        if open {
            if let Some(prev) = out.last_mut() {
                prev.pop();
                prev.truncate(prev.trim_end().len());
                prev.push(' ');
                prev.push_str(t);
            }
            open = t.ends_with('\\');
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let first = t.split_whitespace().next().unwrap_or_default();
        if first == "set"
            && t.split_whitespace()
                .skip(1)
                .all(|a| a.starts_with(['-', '+']) || a == "pipefail")
        {
            continue;
        }
        if matches!(
            first,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "function"
                | "{"
                | "}"
                | "trap"
                | "exec"
        ) || t.ends_with("()")
            || t.contains("() {")
        {
            return Err("its script has control flow Desk doesn't follow".into());
        }
        out.push(t.to_string());
        open = t.ends_with('\\');
    }
    Ok(out)
}

/// Reads scripts in the repository at `top`, only as `origin`'s
/// `default_branch` has them, unchanged.
///
/// That is a speed bump, not a guard: it stops a session editing a script
/// and then asking to run it, but the default branch is whatever was
/// pushed to it. What a script runs is still judged line by line.
pub struct Reader {
    pub top: PathBuf,
    pub default_branch: Option<String>,
    /// The person's home: their own `.npmrc` counts too.
    pub home: PathBuf,
}

/// Whether npm or yarn settings `body` choose the shell scripts run in
/// (`script-shell`, yarn's `scriptShell`): then a script isn't run as
/// Desk read it.
pub fn sets_script_shell(body: &str) -> bool {
    body.lines().any(|l| {
        let k = l
            .trim()
            .split(['=', ':', ' '])
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .to_lowercase();
        matches!(k.as_str(), "script-shell" | "scriptshell" | "script_shell")
    })
}

impl Reader {
    fn git_ok(&self, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&self.top)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// `path`'s body, when it is the repository's, as on `origin`'s default
    /// branch and unchanged since.
    fn unchanged(&self, path: &Path) -> Result<String, String> {
        let real = super::boss::resolve_real(path)
            .ok_or_else(|| "Desk can't find the script it runs".to_string())?;
        let rel = real
            .strip_prefix(&self.top)
            .map_err(|_| "runs a script outside its repository".to_string())?;
        let rel = rel.to_string_lossy();
        let Some(default) = self.default_branch.as_deref() else {
            return Err("Desk can't tell the repository's default branch".into());
        };
        let at = format!("origin/{default}");
        if !self.git_ok(&["cat-file", "-e", &format!("{at}:{rel}")])
            || !self.git_ok(&["diff", "--quiet", &at, "--", &rel])
        {
            return Err(format!(
                "its {rel} isn't as on {at}; a script changed since is the person's to allow"
            ));
        }
        std::fs::read_to_string(&real).map_err(|_| format!("Desk can't read its {rel}"))
    }

    /// `script`'s commands, or why they can't be read.
    pub fn read(&self, script: &Script) -> Read {
        match script {
            Script::Package { dir, name } => {
                for rc in [".npmrc", ".yarnrc", ".yarnrc.yml"] {
                    for at in [dir.as_path(), self.top.as_path(), self.home.as_path()] {
                        if std::fs::read_to_string(at.join(rc)).is_ok_and(|b| sets_script_shell(&b))
                        {
                            return Err(format!(
                                "its {rc} chooses the shell scripts run in; that's the person's to allow"
                            ));
                        }
                    }
                }
                if std::env::var_os("npm_config_script_shell").is_some() {
                    return Err("npm is set to run scripts in another shell".into());
                }
                let body = self.unchanged(&dir.join("package.json"))?;
                package_scripts(&body, name.as_deref())
            }
            Script::Make { dir, target } => {
                let file = ["GNUmakefile", "makefile", "Makefile"]
                    .iter()
                    .map(|n| dir.join(n))
                    .find(|p| p.exists())
                    .ok_or_else(|| "Desk can't find its Makefile".to_string())?;
                make_recipe(&self.unchanged(&file)?, target)
            }
            Script::Shell { path } => shell_lines(&self.unchanged(path)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Acme")
            .env("GIT_AUTHOR_EMAIL", "dev@example.com")
            .env("GIT_COMMITTER_NAME", "Acme")
            .env("GIT_COMMITTER_EMAIL", "dev@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Only a script as `origin`'s default branch has it is read: one
    /// edited since, one new, or one with no default branch known is the
    /// person's.
    #[test]
    fn only_a_script_unchanged_from_the_default_branch_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(dir.path()).unwrap();
        git(&top, &["init", "-q", "-b", "main"]);
        std::fs::write(
            top.join("package.json"),
            r#"{"scripts": {"deploy": "wrangler deploy"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(top.join("scripts")).unwrap();
        std::fs::write(top.join("scripts/release.sh"), "git push origin v1.2.0\n").unwrap();
        git(&top, &["add", "."]);
        git(&top, &["commit", "-q", "-m", "Scripts"]);
        git(&top, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let home = tempfile::tempdir().unwrap();
        let reader = Reader {
            top: top.clone(),
            default_branch: Some("main".into()),
            home: home.path().to_path_buf(),
        };
        let deploy = Script::Package {
            dir: top.clone(),
            name: Some("deploy".into()),
        };
        assert_eq!(reader.read(&deploy).unwrap(), vec!["wrangler deploy"]);
        let release = Script::Shell {
            path: top.join("scripts/release.sh"),
        };
        assert_eq!(
            reader.read(&release).unwrap(),
            vec!["git push origin v1.2.0"]
        );

        // Edited since: the person's.
        std::fs::write(
            top.join("package.json"),
            r#"{"scripts": {"deploy": "wrangler deploy && npm publish"}}"#,
        )
        .unwrap();
        assert!(reader
            .read(&deploy)
            .unwrap_err()
            .contains("isn't as on origin/main"));
        // Committed but not on origin's branch: the person's too.
        git(&top, &["commit", "-q", "-am", "Publish too"]);
        assert!(reader
            .read(&deploy)
            .unwrap_err()
            .contains("isn't as on origin/main"));
        // New, untracked.
        std::fs::write(top.join("scripts/promote.sh"), "true\n").unwrap();
        let new = Script::Shell {
            path: top.join("scripts/promote.sh"),
        };
        assert!(reader
            .read(&new)
            .unwrap_err()
            .contains("isn't as on origin/main"));
        // No default branch known.
        let blind = Reader {
            top: top.clone(),
            default_branch: None,
            home: home.path().to_path_buf(),
        };
        assert!(blind.read(&release).is_err());
        // Outside the repository.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.sh"), "true\n").unwrap();
        let elsewhere = Script::Shell {
            path: outside.path().join("x.sh"),
        };
        assert!(reader.read(&elsewhere).is_err());

        // A project's or the person's npm settings choosing the shell:
        // the person's.
        git(&top, &["checkout", "-q", "--", "package.json"]);
        git(&top, &["reset", "-q", "--hard", "origin/main"]);
        assert!(reader.read(&deploy).is_ok());
        std::fs::write(home.path().join(".npmrc"), "script-shell=/tmp/sh\n").unwrap();
        assert!(reader.read(&deploy).unwrap_err().contains(".npmrc"));
        std::fs::remove_file(home.path().join(".npmrc")).unwrap();
        std::fs::write(top.join(".yarnrc.yml"), "scriptShell: /tmp/sh\n").unwrap();
        assert!(reader.read(&deploy).unwrap_err().contains(".yarnrc.yml"));
        assert!(!sets_script_shell(
            "registry=https://registry.example.com/\n"
        ));
    }

    #[test]
    fn package_scripts_are_read_with_their_pre_and_post() {
        let body = r#"{"scripts": {"prerelease": "npm test", "release": "npm publish", "deploy": "vite build && wrangler deploy", "version": "git add CHANGELOG.md"}}"#;
        assert_eq!(
            package_scripts(body, Some("release")).unwrap(),
            vec!["npm test", "npm publish"]
        );
        assert_eq!(
            package_scripts(body, Some("deploy")).unwrap(),
            vec!["vite build && wrangler deploy"]
        );
        assert_eq!(
            package_scripts(body, None).unwrap(),
            vec!["git add CHANGELOG.md"]
        );
        assert!(package_scripts(body, Some("promote")).is_err());
        assert!(package_scripts("{", Some("release")).is_err());
    }

    #[test]
    fn a_make_target_is_read_with_its_prerequisites() {
        let body = "VERSION := 1.2\n.PHONY: release build\n\nbuild:\n\t@cargo build --release\n\nrelease: build \\\n  test\n\tgit tag v1.2.0\n\t-git push origin v1.2.0\n\ntest:\n\tcargo test\n\npush:\n\tgit push $(REMOTE) main\n";
        assert_eq!(
            make_recipe(body, "release").unwrap(),
            vec![
                "cargo build --release",
                "cargo test",
                "git tag v1.2.0",
                "git push origin v1.2.0"
            ]
        );
        assert!(make_recipe(body, "push").unwrap_err().contains("variables"));
        assert!(make_recipe(body, "deploy")
            .unwrap_err()
            .contains("no deploy target"));
        assert!(make_recipe("include other.mk\nrelease:\n\ttrue\n", "release").is_err());
    }

    #[test]
    fn a_shell_scripts_lines_are_read_and_its_control_flow_is_the_persons() {
        let body = "#!/usr/bin/env bash\nset -euo pipefail\n# Ship it.\nnpm run build\ngit push origin \\\n  v1.2.0\n";
        assert_eq!(
            shell_lines(body).unwrap(),
            vec!["npm run build", "git push origin v1.2.0"]
        );
        assert!(shell_lines("if true; then\n  npm publish\nfi\n").is_err());
        assert!(shell_lines("ship() {\n  npm publish\n}\nship\n").is_err());
    }
}
