//! Which shell commands an answer from elsewhere may allow
//! ([`crate::boss_ask::read`]): only those this engine reads the whole of.
//!
//! Not a denylist. A command is answerable from anywhere only when all of
//! it parses under a strict grammar and each of its programs is one this
//! engine knows, given only what it knows that program to do with it.
//! Everything else is answered in Desk:
//! - words are plain, single-quoted, or double-quoted without `$`, a
//!   backtick or an escape but `\"` and `\\`; commands are joined by `&&`,
//!   `||`, `;`, `|` or a newline; anything else unquoted (`$`, a
//!   backtick, `(`, `)`, `{`, `}`, `<<`, `<(`, a lone `&`, a `#`, a `\`)
//!   isn't read;
//! - a redirect writes only to a plain path inside the session's folder or
//!   `/dev/null`, and reads only from one inside it;
//! - no keyword, `!` or assignment starts a command, and a program is a
//!   bare name from [`PROGRAMS`], never a path;
//! - a program that writes (`cp`, `mv`, `mkdir`, `touch`, `tee`, `rm`)
//!   writes only to plain paths inside the folder; nothing writes after a
//!   `cd`, and a `cd` stays inside the folder.
//!
//! Over-catching costs one answer in Desk. [`crate::never`], the markers
//! of what runs later and the shipping rules apply on top of this.

use std::path::Path;

/// A word as the shell takes it, and whether it has a pattern the shell
/// expands (`*`, `?`, `[` unquoted).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    text: String,
    pattern: bool,
}

/// One command: its words, and the files it redirects to and from.
#[derive(Debug, Default)]
struct Command {
    words: Vec<Word>,
    writes: Vec<Word>,
    reads: Vec<Word>,
}

/// `command` as commands, or none when any of it isn't under the grammar.
fn parse(command: &str) -> Option<Vec<Command>> {
    let mut out = Vec::new();
    let mut cmd = Command::default();
    let mut word: Option<Word> = None;
    let mut chars = command.chars().peekable();
    // The next word is a redirect's target: true to write, false to read.
    let mut target: Option<bool> = None;
    fn end(word: &mut Option<Word>, cmd: &mut Command, target: &mut Option<bool>) {
        if let Some(w) = word.take() {
            match target.take() {
                Some(true) => cmd.writes.push(w),
                Some(false) => cmd.reads.push(w),
                None => cmd.words.push(w),
            }
        }
    }
    fn next(out: &mut Vec<Command>, cmd: &mut Command, target: &Option<bool>) -> Option<()> {
        // A redirect with no target, or an empty command, isn't read.
        if target.is_some() || cmd.words.is_empty() {
            return None;
        }
        out.push(std::mem::take(cmd));
        Some(())
    }
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end(&mut word, &mut cmd, &mut target),
            '\n' | ';' => {
                end(&mut word, &mut cmd, &mut target);
                next(&mut out, &mut cmd, &target)?;
            }
            '|' => {
                end(&mut word, &mut cmd, &mut target);
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                next(&mut out, &mut cmd, &target)?;
            }
            '&' => match chars.peek() {
                Some('&') => {
                    chars.next();
                    end(&mut word, &mut cmd, &mut target);
                    next(&mut out, &mut cmd, &target)?;
                }
                // `&>f`: both streams to a file.
                Some('>') if word.is_none() => {
                    chars.next();
                    if chars.peek() == Some(&'>') {
                        chars.next();
                    }
                    target.is_none().then_some(())?;
                    target = Some(true);
                }
                _ => return None,
            },
            '>' => {
                // `2>`: an fd before it is no word.
                match &word {
                    Some(w) if !w.pattern && w.text.chars().all(|c| c.is_ascii_digit()) => {
                        word = None;
                    }
                    _ => end(&mut word, &mut cmd, &mut target),
                }
                target.is_none().then_some(())?;
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                if chars.peek() == Some(&'&') {
                    // `2>&1`, `>&2`: to another of its streams, and no more.
                    chars.next();
                    let mut fd = String::new();
                    while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                        fd.push(*d);
                        chars.next();
                    }
                    if fd.is_empty() {
                        return None;
                    }
                } else {
                    target = Some(true);
                }
            }
            '<' => {
                end(&mut word, &mut cmd, &mut target);
                if matches!(chars.peek(), Some('<' | '(' | '&')) || target.is_some() {
                    return None;
                }
                target = Some(false);
            }
            '\'' => {
                let w = word.get_or_insert_with(|| Word {
                    text: String::new(),
                    pattern: false,
                });
                loop {
                    match chars.next()? {
                        '\'' => break,
                        q => w.text.push(q),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(|| Word {
                    text: String::new(),
                    pattern: false,
                });
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            e @ ('"' | '\\') => w.text.push(e),
                            _ => return None,
                        },
                        q => w.text.push(q),
                    }
                }
            }
            '$' | '`' | '(' | ')' | '{' | '}' | '#' | '\\' | '!' => return None,
            c => {
                let w = word.get_or_insert_with(|| Word {
                    text: String::new(),
                    pattern: false,
                });
                if matches!(c, '*' | '?' | '[') {
                    w.pattern = true;
                }
                w.text.push(c);
            }
        }
    }
    end(&mut word, &mut cmd, &mut target);
    if target.is_some() {
        return None;
    }
    if !cmd.words.is_empty() {
        out.push(cmd);
    } else if !cmd.writes.is_empty() || !cmd.reads.is_empty() {
        return None;
    }
    (!out.is_empty()).then_some(out)
}

/// The programs a command may be, and what each may be given.
pub const PROGRAMS: &[&str] = &[
    // Reading.
    "ls", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "rg", "find", "pwd", "echo",
    "printf", "true", "false", "which", "file", "stat", "du", "df", "diff", "cmp", "sort", "uniq",
    "cut", "tr", "nl", "tac", "rev", "column", "jq", "date", "uname", "whoami", "id", "tree",
    "basename", "dirname", "realpath", "readlink", "sleep", "seq", "test", "[", "sed",
    // Writing, inside the folder.
    "cp", "mv", "mkdir", "touch", "tee", "rm", "cd", "pushd", "popd",
    // Building and testing what the folder holds.
    "cargo", "npm", "pnpm", "yarn", "bun", "make", "just", "go", "pytest", "tsc", "eslint",
    "prettier", "python", "python3", "node", "git", "gh", "curl",
];

/// Whether `command` is answerable from anywhere: all of it under the
/// grammar, each program one of [`PROGRAMS`] given only what it is known to
/// do with, and every file it writes a plain path inside `cwd`.
pub fn readable(command: &str, cwd: Option<&Path>, home: &Path) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let Some(commands) = parse(command) else {
        return false;
    };
    let inside = |w: &Word| {
        !w.pattern
            && !w.text.is_empty()
            && crate::boss_ask::resolved(&w.text, Some(cwd), home).is_some_and(|at| {
                at.starts_with(crate::boss_ask::real_path(cwd))
                    && !at
                        .strip_prefix(crate::boss_ask::real_path(home))
                        .ok()
                        .is_some_and(|r| {
                            r.components()
                                .next()
                                .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with('.'))
                        })
            })
    };
    let null = |w: &Word| w.text == "/dev/null";
    let moves = commands
        .iter()
        .any(|c| matches!(c.words[0].text.as_str(), "cd" | "pushd" | "popd"));
    let mut writes = false;
    for c in &commands {
        if c.writes.iter().any(|w| !null(w)) {
            writes = true;
        }
        if !c.writes.iter().all(|w| null(w) || inside(w)) || !c.reads.iter().all(inside) {
            return false;
        }
        let program = &c.words[0];
        if (program.pattern && program.text != "[") || !PROGRAMS.contains(&program.text.as_str()) {
            return false;
        }
        let args: Vec<&str> = c.words[1..].iter().map(|w| w.text.as_str()).collect();
        let operands = || c.words[1..].iter().filter(|w| !w.text.starts_with('-'));
        let ok = match program.text.as_str() {
            "cp" | "mv" | "mkdir" | "touch" | "tee" | "rm" => {
                writes = true;
                operands().all(inside)
                    && !args
                        .iter()
                        .any(|a| a.starts_with("--target-directory") || *a == "-t")
            }
            "cd" | "pushd" => operands().all(inside),
            "popd" => args.is_empty(),
            "find" => !args.iter().any(|a| {
                a.starts_with("-exec")
                    || a.starts_with("-ok")
                    || a.starts_with("-fprint")
                    || matches!(*a, "-delete" | "-fls")
            }),
            "rg" => !args.iter().any(|a| a.starts_with("--pre")),
            "sort" => !args
                .iter()
                .any(|a| a.starts_with("--output") || (short(a) && a.contains('o'))),
            // Its second operand is a file it writes.
            "uniq" => operands().count() <= 1,
            "sed" => sed(&args),
            "cargo" => cargo(&args),
            "npm" | "pnpm" | "yarn" | "bun" => js(&args),
            "make" | "just" => !args.iter().any(|a| {
                a.starts_with("-C")
                    || a.starts_with("-f")
                    || a.starts_with("--directory")
                    || a.starts_with("--file")
                    || a.starts_with("--makefile")
                    || a.starts_with("--eval")
                    || a.starts_with("-E")
            }),
            "go" => {
                matches!(
                    args.first(),
                    Some(&("build" | "test" | "vet" | "fmt" | "list" | "version" | "env" | "doc"))
                ) && !args.iter().any(|a| {
                    a.starts_with("-o") || a.starts_with("-exec") || a.starts_with("-toolexec")
                })
            }
            "python" | "python3" => python(c, &inside),
            // A script the folder holds, given first.
            "node" => c
                .words
                .get(1)
                .is_some_and(|s| !s.text.starts_with('-') && inside(s)),
            "pytest" | "tsc" | "eslint" | "prettier" => !args.iter().any(|a| {
                a.starts_with("--out")
                    || a.starts_with("-o")
                    || a.starts_with("--config")
                    || a.starts_with("-c")
                    || a.starts_with("--rootdir")
                    || a.starts_with("-p")
                    || a.starts_with("--plugin")
                    || a.starts_with("--resolve-plugins")
            }),
            "git" => git(&args),
            "gh" => gh(&args),
            "curl" => curl(&args),
            "date" => !args
                .iter()
                .any(|a| a.starts_with("-s") || a.starts_with("--set")),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    // Where it writes after a `cd` isn't followed.
    !(moves && writes)
}

/// Whether `a` is a cluster of short flags (`-la`), not a long one.
fn short(a: &str) -> bool {
    a.starts_with('-') && !a.starts_with("--") && a.len() > 1
}

/// `sed` that only prints or substitutes: never in place, never a script
/// file, and no command that runs, reads or writes (`e`, `w`, `r`, the
/// `e` and `w` flags).
fn sed(args: &[&str]) -> bool {
    let mut scripts = Vec::new();
    let mut it = args.iter();
    let mut first = true;
    while let Some(a) = it.next() {
        match *a {
            "-n" | "--quiet" | "--silent" | "-E" | "-r" | "--regexp-extended" | "-s" | "-u" => {}
            "-e" | "--expression" => scripts.extend(it.next()),
            a if a.starts_with('-') => return false,
            a if first => {
                scripts.push(a);
                first = false;
            }
            _ => {}
        }
        if a.starts_with("-e") || *a == "--expression" {
            first = false;
        }
    }
    !scripts.is_empty() && scripts.iter().all(|s| s.split(';').all(sed_command))
}

/// One `sed` command that prints, deletes or substitutes.
fn sed_command(s: &str) -> bool {
    let s = s.trim();
    // An address: lines, a range, `$`.
    let body = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '$');
    match body {
        "p" | "d" | "q" | "=" | "" => return !s.is_empty(),
        _ => {}
    }
    let Some(rest) = body.strip_prefix('s') else {
        return false;
    };
    let mut chars = rest.chars();
    let Some(delim) = chars.next() else {
        return false;
    };
    if delim.is_alphanumeric() || delim == '\\' || delim.is_whitespace() {
        return false;
    }
    let mut parts = vec![String::new()];
    let mut escaped = false;
    for c in chars {
        if escaped {
            escaped = false;
            parts.last_mut().unwrap().push(c);
        } else if c == '\\' {
            escaped = true;
            parts.last_mut().unwrap().push(c);
        } else if c == delim {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
    }
    parts.len() == 3
        && parts[2]
            .chars()
            .all(|c| matches!(c, 'g' | 'i' | 'I' | 'p' | '0'..='9'))
}

/// `cargo`, but what changes the machine or reaches into its config.
fn cargo(args: &[&str]) -> bool {
    let sub = args
        .iter()
        .find(|a| !a.starts_with('-') && !a.starts_with('+'));
    let flags_ok = !args.iter().any(|a| {
        a.starts_with("--config")
            || a.starts_with("--target-dir")
            || a.starts_with("--root")
            || a.starts_with("--manifest-path")
            || a.starts_with("-Z")
            || a.starts_with("--out-dir")
            || a.starts_with("--artifact-dir")
    });
    flags_ok
        && sub.is_some_and(|s| {
            matches!(
                *s,
                "build"
                    | "b"
                    | "check"
                    | "c"
                    | "test"
                    | "t"
                    | "clippy"
                    | "fmt"
                    | "doc"
                    | "d"
                    | "bench"
                    | "run"
                    | "r"
                    | "metadata"
                    | "tree"
                    | "nextest"
                    | "version"
                    | "update"
                    | "fetch"
                    | "clean"
                    | "add"
                    | "remove"
                    | "search"
            )
        })
}

/// A JavaScript package manager's scripts and installs, not its config,
/// sign-ins or a package run from elsewhere.
fn js(args: &[&str]) -> bool {
    let sub = args.iter().find(|a| !a.starts_with('-'));
    !args.iter().any(|a| {
        a.starts_with("--prefix")
            || a.starts_with("-C")
            || a.starts_with("--dir")
            || a.starts_with("--global")
            || *a == "-g"
            || a.starts_with("--userconfig")
            || a.starts_with("--registry")
    }) && sub.is_some_and(|s| {
        matches!(
            *s,
            "test"
                | "t"
                | "run"
                | "run-script"
                | "install"
                | "i"
                | "ci"
                | "ls"
                | "list"
                | "outdated"
                | "audit"
                | "why"
                | "build"
                | "lint"
                | "add"
                | "remove"
                | "uninstall"
                | "typecheck"
                | "format"
                | "dev"
                | "start"
        )
    })
}

/// `python -m` with a known tool, or a script the folder holds, given
/// first.
fn python(c: &Command, inside: &dyn Fn(&Word) -> bool) -> bool {
    match c.words.get(1).map(|w| w.text.as_str()) {
        Some("-m") => c.words.get(2).is_some_and(|m| {
            matches!(
                m.text.as_str(),
                "pytest" | "unittest" | "mypy" | "ruff" | "black" | "pip" | "venv" | "build"
            )
        }),
        Some(s) if !s.starts_with('-') => inside(&c.words[1]),
        _ => false,
    }
}

/// `git`, with no option before its subcommand (`-c`, `-C`, `--git-dir`,
/// `--exec-path`), a subcommand that only works on the repository, and
/// nothing that writes elsewhere or runs a command (`--output`, `-x`,
/// `--exec`, `--upload-pack`).
fn git(args: &[&str]) -> bool {
    let Some(sub) = args.first() else {
        return false;
    };
    let rest = &args[1..];
    let refused = rest.iter().any(|a| {
        a.starts_with("--output")
            || a.starts_with("--exec")
            || a.starts_with("--upload-pack")
            || a.starts_with("--receive-pack")
            || a.starts_with("--config")
            || a.starts_with("--git-dir")
            || a.starts_with("--work-tree")
            || a.starts_with("--ext-diff")
            || a.starts_with("--template")
            || a.starts_with("--separate-git-dir")
            || a.starts_with("--extcmd")
            || a.starts_with("--tool")
            || *a == "-x"
            || *a == "-o"
            || *a == "-c"
    });
    let known = match *sub {
        "status" | "diff" | "log" | "show" | "branch" | "checkout" | "switch" | "add"
        | "commit" | "restore" | "reset" | "stash" | "fetch" | "pull" | "push" | "merge"
        | "rebase" | "tag" | "rev-parse" | "ls-files" | "blame" | "grep" | "describe"
        | "shortlog" | "cherry-pick" | "revert" | "mv" | "rm" | "rev-list" | "merge-base"
        | "cat-file" | "ls-tree" | "reflog" => true,
        "remote" => rest
            .iter()
            .all(|a| matches!(*a, "-v" | "--verbose" | "show")),
        "config" => {
            rest.iter()
                .any(|a| matches!(*a, "--get" | "--get-all" | "--list" | "-l"))
                || rest.iter().filter(|a| !a.starts_with('-')).count() <= 1
        }
        "worktree" => rest.first() == Some(&"list"),
        _ => false,
    };
    known && !refused
}

/// `gh` but what changes its sign-in, config, keys or extensions.
fn gh(args: &[&str]) -> bool {
    matches!(
        args.first(),
        Some(
            &("pr"
                | "issue"
                | "run"
                | "repo"
                | "workflow"
                | "release"
                | "api"
                | "search"
                | "browse"
                | "status"
                | "label")
        )
    ) && !args.iter().any(|a| {
        matches!(
            *a,
            "clone" | "fork" | "edit" | "delete" | "download" | "set-default"
        ) || a.starts_with("--output")
            || *a == "-o"
    })
}

/// `curl` that only fetches to its output: no file written or uploaded,
/// no body sent, no method but GET, no config file read.
fn curl(args: &[&str]) -> bool {
    !args.iter().any(|a| {
        let long = |p: &str| a.starts_with(p);
        long("--output")
            || long("--remote-name")
            || long("--upload-file")
            || long("--config")
            || long("--data")
            || long("--form")
            || long("--json")
            || long("--request")
            || long("--cookie-jar")
            || long("--dump-header")
            || long("--trace")
            || long("--stderr")
            || long("--libcurl")
            || long("--create-dirs")
            || long("--netrc")
            || (short(a) && a[1..].contains(['o', 'O', 'T', 'K', 'd', 'F', 'X', 'c', 'D', 'b']))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_is_read_whole_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = home.join("acme-api");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        let ok = |c: &str| readable(c, Some(&cwd), &home);
        for c in [
            "cargo build --release",
            "cargo test --all",
            "ls -la",
            "git status && cargo build",
            "cd src && cargo test",
            "cargo build > build.log 2>&1",
            "cargo test 2>/dev/null | tee out.txt",
            "grep -rn 'TODO' src | head -20",
            "rg 'Vec<T>' -g '*.rs'",
            "jq '.items[0]' data.json",
            "sed -n '1,20p' src/main.rs",
            "sed 's/foo/bar/g' notes.txt",
            "git commit -m \"fix: does a -> b work?\"",
            "git log --oneline -5",
            "git push origin main",
            "npm test",
            "pnpm run lint",
            "python3 -m pytest -q",
            "mkdir -p target/x && cp a.rs src/b.rs",
            "curl -sL https://example.com/x.json",
            "[ -f Cargo.toml ] && cargo build",
            "cat src/*.rs",
            "echo done",
        ] {
            assert!(ok(c), "{c}");
        }
        for c in [
            // Shell it doesn't read.
            "if true; then bash -c 'x'; fi",
            "for f in a b; do cp \"$f\" /usr/local/bin/; done",
            "! sh -c x",
            "$(echo sh) -c 'x'",
            "`which python3` -c 'x'",
            "echo $HOME",
            "echo \"$HOME\"",
            "cat <<EOF\nx\nEOF",
            "cat <(ls)",
            "sleep 1 &",
            "echo x # y",
            "echo a\\ b",
            "{ ls; }",
            "(cd .. && ls)",
            "echo 'unbalanced",
            // Programs it doesn't know, or given as a path, or by assignment.
            "bash scripts/test.sh",
            "sh -c 'ls'",
            "python3 -c 'print(1)'",
            "node -e 'x'",
            "awk '{print $2}' f",
            "xargs rm",
            "ssh host ls",
            "scp f host:",
            "rsync -a dist/ host:/srv",
            "./scripts/build.sh",
            "/usr/bin/git status",
            "GIT_SSH_COMMAND=x git fetch",
            "LD_PRELOAD=x cargo build",
            "sudo cargo build",
            "env cargo build",
            "nice -n 5 cargo build",
            "eval ls",
            // What they're given that runs or writes elsewhere.
            "git -c core.fsmonitor=x status",
            "git -C ../other status",
            "git --git-dir=../x log",
            "git diff --output=../x",
            "git rebase -x 'make' main",
            "git config core.hooksPath .hooks",
            "git clone https://example.com/x.git ../x",
            "rg --pre ./x foo",
            "find . -exec rm {} +",
            "find . -delete",
            "sort -o ../x f",
            "uniq a ../b",
            "sed -i 's/a/b/' f",
            "sed -ri 's/a/b/' f",
            "sed 's/x/id/e' f",
            "sed 'w ../x' f",
            "sed -f script.sed f",
            "cargo install ripgrep",
            "cargo build --target-dir ../t",
            "cargo --config x build",
            "npm exec x",
            "npx x",
            "npm config set registry x",
            "npm install -g x",
            "make -C ../x",
            "go build -o ../x",
            "curl -o x https://x",
            "curl -fsSLo x https://x",
            "curl -X POST https://x",
            "curl -d @f https://x",
            "curl -K cfg https://x",
            "gh auth login",
            "gh repo clone x ../x",
            // Writes outside the folder, or after a cd.
            "echo x > ../out",
            "echo x >> ~/.zshrc",
            "echo x > /etc/hosts",
            "echo x > /tmp/notes.txt",
            "cp x /usr/local/bin/x",
            "cp x ../x",
            "touch src/*.rs",
            "mv a ~/b",
            "tee ../x",
            "cd /etc && ls",
            "cd .. && ls",
            "cd src && echo x > y",
            "cat < ~/.ssh/id_rsa",
            "echo x >& ../out",
            "echo x >",
        ] {
            assert!(!ok(c), "{c}");
        }
        // Not knowing its folder, nothing.
        assert!(!readable("ls", None, &home));
    }
}
