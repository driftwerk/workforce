//! Which of a session's requests only read inside its folder, so that the
//! overseer may allow them without the person (`[oversight.act]
//! read_only_approvals`): an allowlist of read tools, and of programs,
//! subcommands and flags known to only read. Anything else is the person's
//! call.
//!
//! Inside the folder is no stronger than the folder: a session started in a
//! home directory reads all of it. A repository's own config can still make
//! a read command run a program (git's `core.fsmonitor`, a diff or textconv
//! driver); it is the session's repository, and the person's.

use std::path::{Component, Path};

use workforce_desk::model::PendingApproval;

const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS", "NotebookRead"];

/// The flags a read-only program may be given, and so the words it reads as
/// operands (paths, patterns, revisions). A flag not listed is refused: a
/// command is read-only only when every word of it is known to be.
///
/// Flags that write (`tree -o`, `git diff --output`), run a program
/// (`rg --pre`, `rg --search-zip`), change refs (`git branch -D`) or name a
/// file to read other than as an operand (`grep -f`) are left out.
struct Flags {
    /// Short flags taking no value, combinable (`-rn`).
    short: &'static str,
    /// Short flags taking a value: the next word, or the rest of the word
    /// (`-A3`, `-rnA3`).
    short_valued: &'static str,
    /// Short flags whose value is optional, and attached when given
    /// (`git diff -U3`): the next word is never theirs.
    short_optional: &'static str,
    /// Long flags taking no value, by name, space-separated.
    long: &'static str,
    /// Long flags taking a value: `--name=value` or the next word.
    long_valued: &'static str,
    /// Long flags whose value is optional, and attached when given
    /// (`--color`, `--color=always`).
    long_optional: &'static str,
    /// A bare count is a flag (`head -20`, `git log -5`).
    counts: bool,
    /// Whether it takes operands at all (`git branch` with one creates a
    /// branch).
    operands: bool,
}

const LS: Flags = Flags {
    short: "1aAdFhilLnRrSt",
    short_valued: "",
    short_optional: "",
    long:
        "all almost-all human-readable recursive reverse classify directory inode dereference size",
    long_valued: "",
    long_optional: "color",
    counts: false,
    operands: true,
};
const CAT: Flags = Flags {
    short: "AbeEnstTuv",
    short_valued: "",
    short_optional: "",
    long: "number number-nonblank show-all show-ends show-tabs squeeze-blank show-nonprinting",
    long_valued: "",
    long_optional: "",
    counts: false,
    operands: true,
};
/// `head` and `tail`, never following a file (`tail -f` waits forever).
const HEAD_TAIL: Flags = Flags {
    short: "qvr",
    short_valued: "nc",
    short_optional: "",
    long: "quiet silent verbose",
    long_valued: "lines bytes",
    long_optional: "",
    counts: true,
    operands: true,
};
const WC: Flags = Flags {
    short: "clmwL",
    short_valued: "",
    short_optional: "",
    long: "bytes chars lines words max-line-length",
    long_valued: "",
    long_optional: "",
    counts: false,
    operands: true,
};
const GREP: Flags = Flags {
    short: "rRniIlLcvwxEFGPohHsqaz",
    short_valued: "ABCemdD",
    short_optional: "",
    long: "recursive dereference-recursive line-number ignore-case files-with-matches files-without-match count invert-match word-regexp line-regexp extended-regexp fixed-strings basic-regexp perl-regexp only-matching no-filename with-filename no-messages quiet silent text null null-data byte-offset",
    long_valued: "include exclude exclude-dir regexp max-count after-context before-context directories devices binary-files label",
    long_optional: "context color colour",
    counts: false,
    operands: true,
};
const RG: Flags = Flags {
    short: "iSsnNlcvwxFUuoHhqL0.",
    short_valued: "egtTABCmMjdEr",
    short_optional: "",
    long: "ignore-case smart-case case-sensitive line-number no-line-number files-with-matches files-without-match count count-matches invert-match word-regexp line-regexp fixed-strings multiline unrestricted only-matching hidden no-ignore no-ignore-vcs no-heading heading with-filename no-filename files type-list json vimgrep column trim quiet follow null stats no-messages binary text crlf one-file-system passthru pretty",
    long_valued: "regexp glob iglob type type-not after-context before-context context max-count max-depth max-filesize max-columns encoding threads sort sortr replace",
    long_optional: "color",
    counts: false,
    operands: true,
};
const PWD: Flags = Flags {
    short: "LP",
    short_valued: "",
    short_optional: "",
    long: "",
    long_valued: "",
    long_optional: "",
    counts: false,
    operands: false,
};
/// `tree`, never writing (`-o`) or rerunning into HTML files (`-R`, `-H`).
const TREE: Flags = Flags {
    short: "adfiFpsughDCnNJXQrt",
    short_valued: "LIP",
    short_optional: "",
    long: "noreport dirsfirst gitignore du prune matchdirs ignore-case",
    long_valued: "filelimit charset sort timefmt",
    long_optional: "",
    counts: false,
    operands: true,
};
/// `file`, never compiling a magic file (`-C`) or looking inside
/// compressed ones (`-z`).
const FILE: Flags = Flags {
    short: "bhiLkNnrs",
    short_valued: "",
    short_optional: "",
    long: "brief mime mime-type mime-encoding dereference no-dereference keep-going no-pad raw special-files",
    long_valued: "",
    long_optional: "",
    counts: false,
    operands: true,
};
const STAT: Flags = Flags {
    short: "LtxlsrqnF",
    short_valued: "c",
    short_optional: "",
    long: "dereference terse file-system",
    long_valued: "format printf",
    long_optional: "",
    counts: false,
    operands: true,
};
const DU: Flags = Flags {
    short: "shkmcax",
    short_valued: "dB",
    short_optional: "",
    long: "summarize human-readable total all one-file-system apparent-size bytes si",
    long_valued: "max-depth block-size exclude threshold",
    long_optional: "",
    counts: false,
    operands: true,
};
const DIFF: Flags = Flags {
    short: "ucrNqiwbBysaEpTt",
    short_valued: "UCxFIW",
    short_optional: "",
    long: "recursive new-file brief ignore-case ignore-all-space ignore-space-change ignore-blank-lines side-by-side report-identical-files text expand-tabs minimal show-c-function suppress-common-lines strip-trailing-cr ignore-tab-expansion",
    long_valued: "exclude ignore-matching-lines show-function-line width label",
    long_optional: "unified context color",
    counts: false,
    operands: true,
};

/// The program `name`'s flags, when it is one that only reads. `git` and
/// `find` have grammars of their own.
fn read_only_program(name: &str) -> Option<&'static Flags> {
    Some(match name {
        "ls" => &LS,
        "cat" => &CAT,
        "head" | "tail" => &HEAD_TAIL,
        "wc" => &WC,
        "grep" => &GREP,
        "rg" => &RG,
        "pwd" => &PWD,
        "tree" => &TREE,
        "file" => &FILE,
        "stat" => &STAT,
        "du" => &DU,
        "diff" => &DIFF,
        _ => return None,
    })
}

const GIT_STATUS: Flags = Flags {
    short: "sbvz",
    short_valued: "",
    short_optional: "",
    long: "short branch long verbose show-stash no-renames ahead-behind no-ahead-behind",
    long_valued: "",
    long_optional: "porcelain untracked-files ignored ignore-submodules column",
    counts: false,
    operands: true,
};
/// What `git diff` shows, never through an external diff or textconv
/// program, and never into a file (`--output`).
const GIT_DIFF: Flags = Flags {
    short: "pusRwbaz",
    short_valued: "",
    short_optional: "U",
    long: "stat shortstat numstat name-only name-status patch no-patch raw cached staged no-color ignore-all-space ignore-space-change ignore-blank-lines no-renames summary check minimal patience histogram full-index binary no-ext-diff no-textconv text merge-base exit-code quiet compact-summary no-prefix",
    long_valued: "diff-filter inter-hunk-context diff-algorithm stat-width",
    long_optional: "unified color word-diff dirstat relative abbrev",
    counts: false,
    operands: true,
};
/// `git log` and `git show`: history and what it changed, under the same
/// limits as `git diff`.
const GIT_LOG: Flags = Flags {
    short: "pusRwbaz",
    short_valued: "SGn",
    short_optional: "U",
    long: "oneline graph all reverse first-parent merges no-merges abbrev-commit no-abbrev-commit follow date-order topo-order author-date-order left-right boundary source full-history no-decorate stat shortstat numstat name-only name-status patch no-patch no-color summary no-renames no-ext-diff no-textconv mailmap use-mailmap children parents ignore-all-space quiet",
    long_valued: "max-count skip since after until before author committer grep diff-filter",
    long_optional: "min-parents max-parents decorate color pretty format abbrev date branches tags remotes unified relative",
    counts: true,
    operands: true,
};
/// `git branch` lists, and nothing more: an operand would create one, and
/// `-d`, `-D`, `-m`, `-c` and the like change refs.
const GIT_BRANCH: Flags = Flags {
    short: "arv",
    short_valued: "",
    short_optional: "",
    long: "list all remotes verbose show-current no-color",
    long_valued: "",
    long_optional: "",
    counts: false,
    operands: false,
};
const GIT_REV_PARSE: Flags = Flags {
    short: "q",
    short_valued: "",
    short_optional: "",
    long: "show-toplevel show-prefix show-cdup git-dir git-common-dir absolute-git-dir is-inside-work-tree is-inside-git-dir is-bare-repository verify quiet symbolic symbolic-full-name all branches tags remotes",
    long_valued: "",
    long_optional: "abbrev-ref short",
    counts: false,
    operands: true,
};
const GIT_LS_FILES: Flags = Flags {
    short: "cdmoskuvfzt",
    short_valued: "x",
    short_optional: "",
    long: "cached deleted modified others ignored stage killed unmerged full-name error-unmatch exclude-standard directory no-empty-directory eol deduplicate",
    long_valued: "exclude",
    long_optional: "",
    counts: false,
    operands: true,
};
const GIT_BLAME: Flags = Flags {
    short: "lstfnwep",
    short_valued: "L",
    short_optional: "",
    long: "porcelain line-porcelain incremental show-name show-number show-email root show-stats",
    long_valued: "date since",
    long_optional: "",
    counts: false,
    operands: true,
};

/// A `git` command's operands, when it is a subcommand that only reads.
/// Options before the subcommand (`-c`, `-C`, `--git-dir`) are refused: they
/// can point git at other repositories or run programs through config.
fn git_operands(args: &[String]) -> Option<Vec<&str>> {
    let (sub, rest) = args.split_first()?;
    let flags = match sub.as_str() {
        "status" => &GIT_STATUS,
        "diff" => &GIT_DIFF,
        "log" | "show" => &GIT_LOG,
        "branch" => &GIT_BRANCH,
        "rev-parse" => &GIT_REV_PARSE,
        "ls-files" => &GIT_LS_FILES,
        "blame" => &GIT_BLAME,
        _ => return None,
    };
    operands(flags, rest)
}

/// `find`'s tests and the actions that only print. `-delete`, `-exec`,
/// `-ok` and the `-fprint` family are not among them.
const FIND_TESTS: &str = "-print -print0 -ls -empty -prune -depth -true -false -not ! -a -and -o -or ( ) -xdev -mount -readable -writable -executable -nouser -nogroup";
const FIND_VALUED: &str = "-name -iname -path -ipath -wholename -iwholename -regex -iregex -regextype -type -xtype -maxdepth -mindepth -mtime -mmin -atime -amin -ctime -cmin -size -perm -user -group -uid -gid -links -inum -fstype";
/// Tests whose value is a file `find` reads, so it is held to the folder.
const FIND_PATH_VALUED: &str = "-newer -anewer -cnewer -samefile";

/// A `find` command's starting points and the files its tests read, when
/// its expression only tests and prints.
fn find_operands(args: &[String]) -> Option<Vec<&str>> {
    let mut words = args.iter().map(String::as_str).peekable();
    let mut read = Vec::new();
    while let Some(start) = words.next_if(|w| !w.starts_with('-') && !matches!(*w, "(" | ")" | "!"))
    {
        read.push(start);
    }
    while let Some(word) = words.next() {
        if listed(FIND_TESTS, word) {
            continue;
        }
        if listed(FIND_VALUED, word) {
            words.next()?;
        } else if listed(FIND_PATH_VALUED, word) {
            read.push(words.next()?);
        } else {
            return None;
        }
    }
    Some(read)
}

/// Whether `name` is one of the space-separated `names`.
fn listed(names: &str, name: &str) -> bool {
    names.split_whitespace().any(|n| n == name)
}

/// The operands of `args`, when every flag in it is one `flags` allows.
fn operands<'a>(flags: &Flags, args: &'a [String]) -> Option<Vec<&'a str>> {
    let mut out = Vec::new();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            out.extend(words.map(String::as_str));
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, _)) => {
                    if !listed(flags.long_valued, name) && !listed(flags.long_optional, name) {
                        return None;
                    }
                }
                None if listed(flags.long, long) || listed(flags.long_optional, long) => {}
                None if listed(flags.long_valued, long) => {
                    words.next()?;
                }
                None => return None,
            }
        } else if let Some(short) = word.strip_prefix('-').filter(|s| !s.is_empty()) {
            if flags.counts && short.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            for (at, flag) in short.char_indices() {
                if flags.short_optional.contains(flag) {
                    break;
                }
                if flags.short_valued.contains(flag) {
                    // The rest of the word is its value, or the next word is.
                    if at + flag.len_utf8() == short.len() {
                        words.next()?;
                    }
                    break;
                }
                if !flags.short.contains(flag) {
                    return None;
                }
            }
        } else {
            out.push(word.as_str());
        }
    }
    (flags.operands || out.is_empty()).then_some(out)
}

/// Whether `path` (as a request names it) stays inside `cwd`.
fn inside(cwd: &Path, path: &str) -> bool {
    let p = Path::new(path);
    if p.components().any(|c| c == Component::ParentDir) || path.starts_with('~') {
        return false;
    }
    !p.is_absolute() || p.starts_with(cwd)
}

/// Whether the shell would make more of `command` than the words it splits
/// into: a glob (`*`, `?`, `[`), a brace list (`{a,b}`), a zsh glob
/// qualifier (`(…)`) or `=program`, a leading tilde, or a comment (`#`,
/// which an interactive zsh doesn't take as one). Each would run words
/// Desk never read: `{--pre=sh,.}` is a flag, `*` can match a file named
/// `--output=x`, and bash's `.*` matches `..`. Quoted, they are the
/// program's to read.
fn expands(command: &str) -> bool {
    let mut quote = None;
    let mut word_start = true;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '*' | '?' | '[' | '{' | '}' | '(' | ')' | '#' => return true,
                '=' | '~' if word_start => return true,
                _ => {}
            },
        }
        word_start = quote.is_none() && c.is_whitespace();
    }
    false
}

/// Whether a session's request only reads, and only inside its folder: a
/// read tool on paths inside it, or a command whose program, subcommand and
/// every flag are known to only read, on operands inside it. Anything Desk
/// can't see all of (a clipped command), can't split as a shell would, or
/// doesn't know, is not.
pub(crate) fn read_only_in(cwd: &str, pending: &PendingApproval) -> bool {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        return false;
    }
    let detail = pending.detail.trim();
    if detail.ends_with('…') {
        return false;
    }
    let name = pending.tool.split('(').next().unwrap_or_default().trim();
    if READ_TOOLS.contains(&name) {
        // The one path it reads; a request that names none is not one Desk
        // can judge.
        return !detail.is_empty() && inside(cwd, detail);
    }
    if name != "Bash" || detail.is_empty() {
        return false;
    }
    if detail.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '\r' | '\\'
        )
    }) || expands(detail)
    {
        return false;
    }
    let Some(words) = shlex::split(detail) else {
        return false;
    };
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let read = match program.as_str() {
        "git" => git_operands(args),
        "find" => find_operands(args),
        other => read_only_program(other).and_then(|flags| operands(flags, args)),
    };
    read.is_some_and(|paths| paths.iter().all(|p| inside(cwd, p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn pending(tool: &str, detail: &str) -> PendingApproval {
        PendingApproval {
            id: Uuid::new_v4(),
            tool: tool.into(),
            detail: detail.into(),
            since: chrono::Utc::now(),
            answerable: true,
        }
    }

    #[test]
    fn only_reads_inside_the_folder_are_read_only() {
        let cwd = "/Users/k/dev/web";
        for (tool, detail) in [
            (
                "Read(/Users/k/dev/web/src/a.ts)",
                "/Users/k/dev/web/src/a.ts",
            ),
            ("Bash(git status)", "git status"),
            ("Bash(ls src)", "ls src"),
            ("Bash(rg TODO src)", "rg TODO src"),
            ("Grep(TODO)", "TODO"),
            ("Bash(rg -n -A3 TODO src)", "rg -n -A3 TODO src"),
            (
                "Bash(rg -g '*.ts' 'fix me' src)",
                "rg -g '*.ts' 'fix me' src",
            ),
            (
                "Bash(grep -rnA3 --include='*.rs' TODO src)",
                "grep -rnA3 --include='*.rs' TODO src",
            ),
            ("Bash(git diff -U5 HEAD~1)", "git diff -U5 HEAD~1"),
            ("Bash(git log --min-parents=2)", "git log --min-parents=2"),
            ("Bash(grep -C 2 TODO src)", "grep -C 2 TODO src"),
            ("Bash(ls 'src/*.ts')", "ls 'src/*.ts'"),
            (
                "Bash(rg 'a{2,3}' \"src/[a-z]*\")",
                "rg 'a{2,3}' \"src/[a-z]*\"",
            ),
            ("Bash(head -20 src/a.ts)", "head -20 src/a.ts"),
            ("Bash(tail -n 5 log.txt)", "tail -n 5 log.txt"),
            (
                "Bash(find . -name '*.rs' -type f)",
                "find . -name '*.rs' -type f",
            ),
            ("Bash(tree -L 2 src)", "tree -L 2 src"),
            ("Bash(git log --oneline -5)", "git log --oneline -5"),
            ("Bash(git log -n 5 --format=%h)", "git log -n 5 --format=%h"),
            (
                "Bash(git diff --stat HEAD~1 -- src)",
                "git diff --stat HEAD~1 -- src",
            ),
            ("Bash(git show HEAD:src/a.ts)", "git show HEAD:src/a.ts"),
            ("Bash(git branch -a)", "git branch -a"),
            ("Bash(git branch --list -vv)", "git branch --list -vv"),
        ] {
            assert!(read_only_in(cwd, &pending(tool, detail)), "{detail}");
        }
        for (tool, detail) in [
            ("Read(/etc/passwd)", "/etc/passwd"),
            ("Read(../secrets)", "../secrets"),
            ("Bash(git push)", "git push"),
            ("Bash(rm -rf target)", "rm -rf target"),
            ("Bash(cat a | sh)", "cat a | sh"),
            ("Bash(ls > out)", "ls > out"),
            ("Bash(find . -delete)", "find . -delete"),
            ("Bash(cat ~/.ssh/id)", "cat ~/.ssh/id"),
            ("Bash(npm install)", "npm install"),
            ("Edit(src/a.ts)", "src/a.ts"),
            ("Bash(ls aaaa…)", "ls aaaa…"),
        ] {
            assert!(!read_only_in(cwd, &pending(tool, detail)), "{detail}");
        }
    }

    /// Commands made only of read-only programs that still write, run a
    /// program or change refs through a flag or an operand: each was
    /// allowed while flags went unread.
    #[test]
    fn a_flag_that_writes_or_runs_something_is_not_read_only() {
        let cwd = "/Users/k/dev/web";
        for detail in [
            "rg --pre=sh TODO .",
            "rg --pre sh TODO .",
            "rg --pre-glob '*' TODO .",
            "rg -z TODO .",
            "git diff --output=out.txt",
            "git log --output out.txt",
            "git show --output=out.txt HEAD",
            "git diff --ext-diff",
            "git branch -D main",
            "git branch -m old new",
            "git branch new-branch",
            "git -c core.pager=sh log",
            "find . -fprint0 out",
            "find . -name x -fprint out",
            "tree -o out.txt",
            "tree -R -H . src",
            "tail -f log.txt",
            "grep -f patterns.txt src",
            "file -C -m magic",
            "ls -l 'unclosed",
        ] {
            let tool = format!("Bash({detail})");
            assert!(!read_only_in(cwd, &pending(&tool, detail)), "{detail}");
        }
    }

    /// A flag whose value is optional takes none from the next word, so
    /// that word is read as git or grep will read it: as a flag, or as a
    /// path.
    #[test]
    fn an_optional_value_is_never_the_next_word() {
        let cwd = "/Users/k/dev/web";
        for detail in [
            "git diff -U --output=out.txt",
            "git log -U --output=out.txt",
            "git show -pU --output=out.txt HEAD",
            "git log --min-parents --output=out.txt",
            "git log --max-parents --output out.txt",
            "grep -e secret --context /etc/passwd",
        ] {
            let tool = format!("Bash({detail})");
            assert!(!read_only_in(cwd, &pending(&tool, detail)), "{detail}");
        }
    }

    /// What the shell expands, unquoted, makes words Desk never split: a
    /// brace list that is a flag or a path outside, a glob that matches a
    /// file named as a flag, or `..`, a zsh qualifier that runs code, a
    /// comment an interactive zsh reads on.
    #[test]
    fn what_the_shell_would_expand_is_not_read_only() {
        let cwd = "/Users/k/dev/web";
        for detail in [
            "rg -e x {--pre=sh,.}",
            "git log {--output=out.txt,HEAD}",
            "cat {/etc/passwd,a}",
            "ls {..,src}",
            "rg x *",
            "find * -name x",
            "grep -r secret .*",
            "cat src/?",
            "ls [.][.]",
            "ls *(e:'touch x':)",
            "cat =ls",
            "cat ~/.ssh/id_rsa",
            "rg x #a --pre=sh",
            "ls src\rrm -rf x",
        ] {
            let tool = format!("Bash({detail})");
            assert!(!read_only_in(cwd, &pending(&tool, detail)), "{detail}");
        }
    }

    /// A read tool is judged by the one path it names; one that names none,
    /// or a session whose folder Desk doesn't know, can't be judged.
    #[test]
    fn a_read_with_no_path_or_folder_is_not_read_only() {
        assert!(!read_only_in(
            "/Users/k/dev/web",
            &pending("NotebookRead", "")
        ));
        assert!(!read_only_in(
            "",
            &pending("Read(/etc/passwd)", "/etc/passwd")
        ));
        assert!(!read_only_in("src", &pending("Bash(ls)", "ls")));
        assert!(!read_only_in(
            "/Users/k/dev/web",
            &pending(
                "Read(/Users/k/dev/web/a ../../../etc/passwd)",
                "/Users/k/dev/web/a ../../../etc/passwd"
            )
        ));
    }
}
