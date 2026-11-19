//! Whether a shell command a boss that prefers sessions runs is coding work
//! (a session's) or a check, lookup or op (its own): read step by step as
//! the shell would split it, quotes and heredocs kept whole.
//!
//! Coding work is a build, test or install run inside code, a change made
//! with git there, an edit in place of a file inside code, or a write,
//! move or delete of a path inside code. A command run on another machine
//! over ssh is that machine's ops, and is never read; what it writes here
//! is. This is a guardrail for a boss told its way of working, not a
//! sandbox: exec containment and the policy engine still judge every
//! command.

use std::path::{Path, PathBuf};

/// A word of a step, and whether any of it was quoted (a quoted `>` is
/// no redirect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Word {
    pub text: String,
    pub quoted: bool,
}

/// `command` split into its steps (at an unquoted `;`, `&`, `|` or new
/// line) and each into its words, quotes taken off. A heredoc's body is
/// text, not steps.
pub(super) fn steps(command: &str) -> Vec<Vec<Word>> {
    let mut steps = Vec::new();
    let mut step: Vec<Word> = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut in_word = false;
    // Heredoc delimiters still to skip to, once the line ends.
    let mut heredocs: Vec<String> = Vec::new();
    let mut chars = command.chars().peekable();
    let end_word =
        |step: &mut Vec<Word>, word: &mut String, quoted: &mut bool, in_word: &mut bool| {
            if *in_word {
                step.push(Word {
                    text: std::mem::take(word),
                    quoted: *quoted,
                });
            }
            *quoted = false;
            *in_word = false;
        };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                quoted = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                quoted = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        word.push(n);
                    }
                }
            }
            '<' if !in_word && chars.peek() == Some(&'<') => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    // A here-string: its word is text, read as one.
                    chars.next();
                    continue;
                }
                if chars.peek() == Some(&'-') {
                    chars.next();
                }
                while chars.peek().is_some_and(|c| *c == ' ') {
                    chars.next();
                }
                let mut delim = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')') {
                        break;
                    }
                    chars.next();
                    if !matches!(c, '\'' | '"' | '\\') {
                        delim.push(c);
                    }
                }
                if !delim.is_empty() {
                    heredocs.push(delim);
                }
            }
            '\n' | ';' | '&' | '|' => {
                end_word(&mut step, &mut word, &mut quoted, &mut in_word);
                // `2>&1`, `>&2`: a descriptor, not a step.
                if c == '&'
                    && step
                        .last()
                        .is_some_and(|w| !w.quoted && w.text.ends_with('>'))
                {
                    let mut fd = String::new();
                    while chars
                        .peek()
                        .is_some_and(|c| c.is_ascii_digit() || *c == '-')
                    {
                        fd.push(chars.next().unwrap_or_default());
                    }
                    if let Some(w) = step.last_mut() {
                        w.text.push('&');
                        w.text.push_str(&fd);
                    }
                    continue;
                }
                if !step.is_empty() {
                    steps.push(std::mem::take(&mut step));
                }
                if c == '\n' && !heredocs.is_empty() {
                    // The body, to each delimiter's own line.
                    for delim in std::mem::take(&mut heredocs) {
                        let mut line = String::new();
                        for c in chars.by_ref() {
                            if c == '\n' {
                                if line.trim() == delim {
                                    break;
                                }
                                line.clear();
                            } else {
                                line.push(c);
                            }
                        }
                    }
                }
            }
            c if c.is_whitespace() => end_word(&mut step, &mut word, &mut quoted, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut step, &mut word, &mut quoted, &mut in_word);
    if !step.is_empty() {
        steps.push(step);
    }
    steps
}

/// Where a word that names a path points, from `here`: `~` and `$HOME`
/// are the person's home.
fn path_of(word: &str, here: &Path) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let (Some(rest), Some(home)) = (word.strip_prefix(prefix), home()) {
            return home.join(rest);
        }
    }
    if matches!(word, "~" | "$HOME" | "${HOME}") {
        if let Some(home) = home() {
            return home;
        }
    }
    here.join(word)
}

/// The words before the program itself that only run it: an env
/// assignment, a wrapper (`env`, `time`, `sudo -u x`, `timeout 30`), a
/// subshell's `(` or a group's `{`.
fn program_at(words: &[Word]) -> usize {
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let t = w.text.trim_start_matches(['(', '{']);
        if t.is_empty() {
            i += 1;
            continue;
        }
        let assignment = !w.quoted
            && t.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
        let wrapper = matches!(
            t,
            "env"
                | "time"
                | "nice"
                | "nohup"
                | "command"
                | "exec"
                | "sudo"
                | "doas"
                | "xargs"
                | "stdbuf"
        );
        if assignment || wrapper {
            i += 1;
            // A wrapper's own flags, and the value the common ones take.
            while let Some(f) = words.get(i).filter(|f| f.text.starts_with('-')) {
                i += 1;
                if matches!(f.text.as_str(), "-u" | "-g" | "-n" | "-I" | "-P") {
                    i += 1;
                }
            }
            continue;
        }
        if t == "timeout" {
            i += 1;
            while words.get(i).is_some_and(|f| f.text.starts_with('-')) {
                i += 1;
            }
            // Its duration.
            i += 1;
            continue;
        }
        break;
    }
    i
}

/// Whether `command`, run in `here`, is coding work; `touches` says
/// whether a path is inside code.
pub(super) fn is_code(command: &str, here: &Path, touches: &dyn Fn(&Path) -> bool) -> bool {
    let mut here = here.to_path_buf();
    for words in steps(command) {
        // Writes by redirect are this machine's, whatever the program.
        let mut operands: Vec<&Word> = Vec::new();
        let mut redirected = false;
        let mut skip = false;
        for (i, w) in words.iter().enumerate() {
            if std::mem::take(&mut skip) {
                continue;
            }
            let t = w.text.trim_start_matches(|c: char| c.is_ascii_digit());
            let target = (!w.quoted)
                .then(|| t.strip_prefix(">>").or_else(|| t.strip_prefix('>')))
                .flatten();
            match target {
                Some(rest) if rest.starts_with('&') => {}
                Some("") => {
                    skip = true;
                    if let Some(next) = words.get(i + 1) {
                        redirected |= touches(&path_of(&next.text, &here));
                    }
                }
                Some(rest) => redirected |= touches(&path_of(rest, &here)),
                None => operands.push(w),
            }
        }
        if redirected {
            return true;
        }
        let at = program_at(&operands.iter().map(|w| (*w).clone()).collect::<Vec<_>>());
        let Some(program) = operands.get(at) else {
            continue;
        };
        let program = program
            .text
            .trim_start_matches(['(', '{'])
            .trim_end_matches([')', '}'])
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let args: Vec<&str> = operands[at + 1..].iter().map(|w| w.text.as_str()).collect();
        let files = |args: &[&str]| -> Vec<String> {
            args.iter()
                .filter(|a| !a.starts_with('-') && !a.is_empty())
                .map(|a| a.to_string())
                .collect()
        };
        let inside = |word: &str| touches(&path_of(word, &here));
        let in_code = touches(&here);
        let mut moved_to = None;
        let code = match program.as_str() {
            // Another machine's: what runs there is never read here.
            "ssh" | "mosh" => false,
            "cd" | "pushd" => {
                moved_to = Some(match files(&args).first() {
                    Some(to) => path_of(to.trim_end_matches([')', '}']), &here),
                    None => std::env::var_os("HOME").map_or(here.clone(), PathBuf::from),
                });
                false
            }
            "git" => {
                // `-C dir` moves where it runs; `-c k=v` and flags are its own.
                let mut place = here.clone();
                let mut sub = None;
                let mut i = 0;
                while let Some(a) = args.get(i) {
                    match *a {
                        "-C" => {
                            if let Some(to) = args.get(i + 1) {
                                place = path_of(to, &place);
                            }
                            i += 2;
                        }
                        "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
                        a if a.starts_with('-') => i += 1,
                        a => {
                            sub = Some(a);
                            break;
                        }
                    }
                }
                let changes = matches!(
                    sub,
                    Some(
                        "commit"
                            | "push"
                            | "pull"
                            | "apply"
                            | "am"
                            | "rebase"
                            | "reset"
                            | "checkout"
                            | "switch"
                            | "restore"
                            | "merge"
                            | "cherry-pick"
                            | "revert"
                            | "clean"
                            | "add"
                            | "rm"
                            | "mv"
                    )
                ) || (sub == Some("stash")
                    && !matches!(args.get(i + 1).copied(), Some("list" | "show")));
                changes && touches(&place)
            }
            "cargo" => {
                let sub = args
                    .iter()
                    .find(|a| !a.starts_with('-') && !a.starts_with('+'));
                in_code
                    && matches!(
                        sub.copied(),
                        Some(
                            "build"
                                | "b"
                                | "test"
                                | "t"
                                | "run"
                                | "r"
                                | "check"
                                | "c"
                                | "clippy"
                                | "fmt"
                                | "install"
                                | "add"
                                | "remove"
                                | "update"
                                | "fix"
                                | "bench"
                                | "nextest"
                        )
                    )
            }
            "npm" | "pnpm" | "yarn" | "bun" => {
                in_code
                    && matches!(
                        args.first().copied(),
                        Some(
                            "run"
                                | "build"
                                | "test"
                                | "t"
                                | "install"
                                | "i"
                                | "ci"
                                | "add"
                                | "remove"
                                | "exec"
                                | "dlx"
                                | "x"
                        )
                    )
            }
            "go" => {
                in_code
                    && match args.first().copied() {
                        Some("build" | "test" | "run" | "generate" | "install" | "get") => true,
                        Some("mod") => matches!(
                            args.get(1).copied(),
                            Some("tidy" | "download" | "edit" | "vendor")
                        ),
                        _ => false,
                    }
            }
            "pip" | "pip3" | "uv" | "poetry" => {
                in_code
                    && args.iter().find(|a| !a.starts_with('-')).is_some_and(|a| {
                        matches!(
                            *a,
                            "install" | "add" | "remove" | "sync" | "run" | "uninstall"
                        )
                    })
            }
            "make" | "cmake" | "ninja" | "bazel" | "gradle" | "mvn" | "npx" | "tsc" | "pytest"
            | "patch" => in_code,
            "sed" => {
                let in_place = args.iter().any(|a| {
                    *a == "--in-place"
                        || a.starts_with("--in-place=")
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
                });
                in_place && files(&args).iter().any(|f| inside(f))
            }
            "perl" => {
                let in_place = args
                    .iter()
                    .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('i'));
                in_place && files(&args).iter().any(|f| inside(f))
            }
            "tee" | "rm" | "mv" | "touch" | "mkdir" | "rmdir" | "unlink" => {
                files(&args).iter().any(|f| inside(f))
            }
            // What they write is where they copy to: after `-t`, else the last.
            "cp" | "ln" | "install" | "rsync" => {
                let to = args
                    .iter()
                    .position(|a| *a == "-t")
                    .and_then(|i| args.get(i + 1).map(|t| t.to_string()))
                    .or_else(|| files(&args).last().cloned());
                to.is_some_and(|t| inside(&t))
            }
            // Their first operand is a mode, an owner or a size.
            "chmod" | "chown" | "chgrp" => files(&args).iter().skip(1).any(|f| inside(f)),
            "truncate" => {
                let size = args.iter().position(|a| *a == "-s").map(|i| i + 1);
                args.iter()
                    .enumerate()
                    .filter(|(i, a)| Some(*i) != size && !a.starts_with('-'))
                    .any(|(_, f)| inside(f))
            }
            _ => false,
        };
        if code {
            return true;
        }
        if let Some(to) = moved_to {
            here = to;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(command: &str) -> Vec<Vec<String>> {
        steps(command)
            .into_iter()
            .map(|s| s.into_iter().map(|w| w.text).collect())
            .collect()
    }

    #[test]
    fn a_command_splits_into_its_steps_with_quotes_and_heredocs_kept_whole() {
        assert_eq!(
            texts("gh issue comment 12 --body \"done; cargo test passes\" && git log"),
            vec![
                vec![
                    "gh",
                    "issue",
                    "comment",
                    "12",
                    "--body",
                    "done; cargo test passes"
                ],
                vec!["git", "log"],
            ]
        );
        assert_eq!(
            texts("cargo test 2>&1 | tail"),
            vec![vec!["cargo", "test", "2>&1"], vec!["tail"]]
        );
        assert_eq!(
            texts("gh pr create --body \"$(cat <<'EOF'\nran cargo test\nEOF\n)\""),
            vec![vec![
                "gh",
                "pr",
                "create",
                "--body",
                "$(cat <<'EOF'\nran cargo test\nEOF\n)"
            ]]
        );
        assert_eq!(
            texts("cat <<EOF > notes.md\nmake install; rm -rf src\nEOF\nls"),
            vec![vec!["cat", ">", "notes.md"], vec!["ls"]]
        );
        assert_eq!(
            texts("awk '$3 > 100' log"),
            vec![vec!["awk", "$3 > 100", "log"]]
        );
    }

    /// `repo` is code; everything else isn't.
    fn code(command: &str, here: &str) -> bool {
        let touches = |p: &Path| p.starts_with("/w/repo");
        is_code(command, Path::new(here), &touches)
    }

    #[test]
    fn builds_git_changes_and_writes_inside_code_are_coding_work() {
        for (command, here) in [
            ("cargo build --release", "/w/repo"),
            ("cargo +nightly test", "/w/repo"),
            ("timeout 600 cargo test", "/w/repo"),
            ("FOO=1 sudo -u dev make", "/w/repo"),
            ("(cd sub && make)", "/w/repo"),
            ("npm run build", "/w/repo"),
            ("git commit -am 'fix'", "/w/repo"),
            ("git -C /w/repo commit -m x", "/w"),
            ("git pull", "/w/repo"),
            ("cd /w/repo && rm -rf src", "/w"),
            ("sed -i 's/a/b/' src/lib.rs", "/w/repo"),
            ("sed -Ei 's/a/b/' /w/repo/x", "/w"),
            ("perl -pi -e 's/a/b/' /w/repo/x", "/w"),
            ("echo x > src/lib.rs", "/w/repo"),
            ("echo x >>/w/repo/a", "/w"),
            ("make 2>/w/repo/err.log", "/w"),
            ("cat a | tee src/new.rs", "/w/repo"),
            ("cp /tmp/a.rs src/a.rs", "/w/repo"),
            ("ssh devbox 'cat f' > /w/repo/out.rs", "/w"),
            ("ssh devbox uptime && cargo build", "/w/repo"),
            ("pip install -e .", "/w/repo"),
        ] {
            assert!(code(command, here), "{command} in {here} ran");
        }
    }

    #[test]
    fn checks_lookups_and_ops_are_its_own() {
        for (command, here) in [
            ("git log -5", "/w/repo"),
            ("git status && git diff", "/w/repo"),
            ("git stash list", "/w/repo"),
            (
                "gh issue comment 12 --body \"done; cargo test passes\"",
                "/w/repo",
            ),
            (
                "gh pr create --body \"$(cat <<'EOF'\nran npm install && make install\nEOF\n)\"",
                "/w/repo",
            ),
            ("awk '$3 > 100' log", "/w/repo"),
            ("jq '.[] | select(.x > 3)' f.json", "/w/repo"),
            ("git log --format='%h > %s'", "/w/repo"),
            ("cargo test 2>&1 >/dev/null", "/w/elsewhere"),
            ("cd /tmp && rm -rf build", "/w/repo"),
            ("chmod +x /tmp/s.sh", "/w/repo"),
            ("cp src/a.rs /tmp/", "/w/repo"),
            ("ln -s src/x /tmp/l", "/w/repo"),
            ("truncate -s 0 /tmp/f", "/w/repo"),
            ("echo x > ~/notes.md", "/w/repo"),
            ("cd ~/x && ssh devbox 'cargo build && make install'", "/w"),
            ("timeout 30 ssh devbox 'make'", "/w/repo"),
            ("pip list", "/w/repo"),
            ("go mod graph", "/w/repo"),
            ("kubectl get pods | grep api", "/w/repo"),
            ("curl -s 'https://x.io/a?b=1&c=2'", "/w/repo"),
            ("docker ps", "/w/repo"),
            ("", "/w/repo"),
        ] {
            assert!(!code(command, here), "{command} in {here} refused");
        }
    }
}
