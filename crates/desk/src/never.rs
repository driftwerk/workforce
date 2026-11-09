//! What no boss does, at any level, Act fully included: one checked list,
//! looked at before the level is (remote-bosses design §8.3).
//!
//! [`ALL`] is the list. Each entry says why in the person's words
//! ([`Never::why`]) and where else it is held ([`Never::held`]), so the list
//! can't drift from the code. Two readers check against it, both pure:
//!
//! - [`command`]: a command a session asks to run, as a boss would allow it.
//!   [`crate::computer::boss::judge`] calls it first, so nothing it opens
//!   (Act fully's shipping in the named repositories) reaches past it.
//! - [`said`]: what a session asks a boss, or the boss answers, in words.
//!   The hub reads both before letting an answer go without the person.
//!
//! Both only ever narrow what a boss does: what they flag goes to the
//! person. They read loosely on purpose (a mention is enough), since the
//! cost of a false catch is one question for the person.

use std::path::{Component, Path, PathBuf};

/// One thing no boss does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Never {
    /// Its own limits: the level, what it may ship in, its budget, who
    /// the boss is.
    OwnLimits,
    /// Money: paying, buying, refunding, raising a budget, releasing a
    /// breaker.
    Money,
    /// Deleting outside the session's own folder and scratch space.
    DeleteOutside,
    /// Rewriting history: a force push, a filter of history.
    RewriteHistory,
    /// Deleting branches, tags, releases or repositories on the remote.
    DeleteRemote,
    /// Publishing to a package registry (crates.io, npm, PyPI, …): it
    /// can't be taken back.
    Registry,
    /// Reading or exporting secrets and credentials.
    Secrets,
    /// New ways out: a new remote, a push to an address, an upload.
    WaysOut,
    /// Starting a session that doesn't ask before it acts.
    Permissionless,
    /// Signing in or out, switching accounts, trusting a host, uninstalling.
    Accounts,
    /// Forgetting what Desk remembers, deleting a group, board or plan's
    /// history, or saving an "always allow" for the person.
    Records,
}

/// Every hard limit, in the order the design lists them.
pub const ALL: [Never; 11] = [
    Never::OwnLimits,
    Never::Money,
    Never::DeleteOutside,
    Never::RewriteHistory,
    Never::DeleteRemote,
    Never::Registry,
    Never::Secrets,
    Never::WaysOut,
    Never::Permissionless,
    Never::Accounts,
    Never::Records,
];

impl Never {
    /// Why, as the person reads it after "it": "it publishes to a package
    /// registry, which can't be undone".
    pub fn why(self) -> &'static str {
        match self {
            Self::OwnLimits => "changes what the boss itself may do",
            Self::Money => "spends or moves money",
            Self::DeleteOutside => "deletes outside the session's own folder",
            Self::RewriteHistory => "force-pushes or rewrites history, which can't be undone",
            Self::DeleteRemote => {
                "deletes branches, tags or releases on the remote, which can't be undone"
            }
            Self::Registry => "publishes to a package registry, which can't be undone",
            Self::Secrets => "reads or exports secrets or credentials",
            Self::WaysOut => "opens a new way out of the repository",
            Self::Permissionless => "starts something that won't ask before it acts",
            Self::Accounts => "signs in or out, switches accounts, or changes the machine",
            Self::Records => "forgets or deletes the person's records",
        }
    }

    /// Where else it is held, besides this list.
    pub fn held(self) -> &'static str {
        match self {
            Self::OwnLimits => {
                "config.toml's [oversight] is human-only (config_tool); Desk's data is withheld from judge"
            }
            Self::Money => "no crew tool raises a budget or continues a breaker",
            Self::DeleteOutside => "judge: deletes only inside the own folder or scratch",
            Self::RewriteHistory => "judge: push refuses --force, -f and +refspecs",
            Self::DeleteRemote => "judge: push refuses --delete, --prune and :refspecs",
            Self::Registry => "judge: cargo, npm, yarn, uv publish and twine, gem refuse",
            Self::Secrets => "judge: boss::secret withholds where secrets are kept",
            Self::WaysOut => "judge: remote, config and curl uploads refuse",
            Self::Permissionless => "crew.start_session takes the engine's default mode only",
            Self::Accounts => "judge: an unknown program or subcommand is the person's",
            Self::Records => "no crew tool forgets memory; Always is the person's (boss_decides)",
        }
    }
}

/// Where a command runs, for what it deletes.
pub struct Where<'a> {
    /// Where it starts; `None`: not known.
    pub cwd: Option<&'a Path>,
    /// The session's own folder, when it has one.
    pub own: Option<&'a Path>,
    pub home: &'a Path,
    /// Scratch space a delete may clear: `/tmp`.
    pub scratch: &'a [PathBuf],
}

// ── Commands ─────────────────────────────────────────────────────────────

/// The commands of `text`: each a list of its words, quotes taken off.
/// What runs inside `$( … )`, backticks and parentheses is a command of its
/// own; redirections are words (a file read or written is named either
/// way).
fn commands(text: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    let end_word = |word: &mut Option<String>, words: &mut Vec<String>| {
        if let Some(w) = word.take() {
            words.push(w);
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    word.get_or_insert_with(String::new).push(n);
                }
            }
            (Some(_), c) => word.get_or_insert_with(String::new).push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        word.get_or_insert_with(String::new).push(n);
                    }
                }
            }
            (None, ' ' | '\t' | '<' | '>') => end_word(&mut word, &mut words),
            (None, ';' | '&' | '|' | '\n' | '(' | ')' | '`' | '{' | '}') => {
                end_word(&mut word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            (None, '$') if chars.peek() == Some(&'(') => {
                end_word(&mut word, &mut words);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            (None, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    end_word(&mut word, &mut words);
    if !words.is_empty() {
        out.push(words);
    }
    out
}

/// A program's name without its folder: `/bin/rm` is `rm`.
fn program(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

/// `words` from the program on: variable assignments and wrappers
/// (`sudo`, `env A=b`, `nice`, `timeout 5`, `xargs`, …) before it skipped.
fn from_program(words: &[String]) -> &[String] {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let assignment = w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        match program(w) {
            _ if assignment => i += 1,
            "sudo" | "doas" | "nice" | "nohup" | "time" | "command" | "exec" | "builtin"
            | "xargs" | "caffeinate" | "stdbuf" => {
                i += 1;
                // Their own flags.
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    i += 1;
                }
            }
            "env" if i + 1 < words.len() => {
                i += 1;
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    i += 1;
                }
            }
            "timeout" | "gtimeout" => {
                i += 1;
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    i += 1;
                }
                // The duration.
                i += 1;
            }
            _ => break,
        }
    }
    &words[i.min(words.len())..]
}

/// Whether `name` (a word, a path) names a file credentials are kept in.
fn secret_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    let examples = [".example", ".sample", ".template", ".dist", ".defaults"];
    let env_file =
        (base == ".env" || base.starts_with(".env.") || base.ends_with(".env") || base == ".envrc")
            && !examples.iter().any(|e| base.ends_with(e));
    // Source and docs are code about credentials, not credentials.
    let code = [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".py", ".go", ".rb", ".java", ".kt", ".swift",
        ".c", ".h", ".cpp", ".cs", ".php", ".md", ".css", ".html", ".sql", ".snap", ".test",
        ".spec",
    ]
    .iter()
    .any(|e| base.ends_with(e));
    let named = !code
        && (base.contains("creds")
            || base.contains("credential")
            || base.starts_with("secret")
            || base.starts_with("password")
            || base == "token"
            || base.ends_with(".token")
            || base == "token.json");
    env_file
        || named
        || [
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".pgpass",
            ".git-credentials",
            "known_hosts",
        ]
        .contains(&base)
        || base.starts_with("id_rsa")
        || base.starts_with("id_ed25519")
        || base.starts_with("id_ecdsa")
        || base.ends_with(".pem")
        || base.ends_with(".p12")
        || base.ends_with(".keychain")
        || base.ends_with(".keychain-db")
        || lower.contains(".aws/")
        || lower.contains(".ssh/")
        || lower.contains(".gnupg")
        || lower.contains("/keychains")
        || lower.contains(".docker/config.json")
        || lower.contains(".kube/config")
}

/// Whether `w` expands a variable that holds a secret: `$GITHUB_TOKEN`,
/// `${AWS_SECRET_ACCESS_KEY}`.
fn secret_variable(w: &str) -> bool {
    let Some(at) = w.find('$') else {
        return false;
    };
    let name: String = w[at + 1..]
        .trim_start_matches('{')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|s| name.contains(s))
}

/// `raw` as a path where it is read: `~` and `$HOME` the home, relative to
/// `cwd`, its `.` and `..` folded. `None` when that can't be told.
fn path_at(raw: &str, at: &Where<'_>) -> Option<PathBuf> {
    let text = raw
        .strip_prefix("${HOME}")
        .or_else(|| raw.strip_prefix("$HOME"))
        .map_or(raw.to_string(), |rest| format!("~{rest}"));
    if text.contains('$') || text.contains('*') || text.contains('?') || text.contains('[') {
        return None;
    }
    let joined = if text == "~" {
        at.home.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        at.home.join(rest)
    } else if text.starts_with('~') {
        return None;
    } else if Path::new(&text).is_absolute() {
        PathBuf::from(&text)
    } else {
        at.cwd?.join(&text)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    Some(out)
}

/// Whether deleting `raw` stays inside the session's own folder or scratch
/// space (never the folder itself, nor the home).
fn deletes_inside(raw: &str, at: &Where<'_>) -> bool {
    let Some(p) = path_at(raw, at) else {
        return false;
    };
    if p == at.home || at.home.starts_with(&p) {
        return false;
    }
    let under = |root: &Path| p.starts_with(root) && p != root;
    at.own.is_some_and(under) || at.scratch.iter().any(|t| under(t))
}

/// Whether `words`, a git push's arguments, push to an address rather than
/// a named remote.
fn push_to_address(operands: &[&str]) -> bool {
    operands
        .first()
        .is_some_and(|r| r.contains("://") || r.contains('@') || r.contains(':') || r.contains('/'))
}

/// The hard limit one command (its words, wrappers skipped) runs into.
fn one(words: &[String], at: &Where<'_>, depth: usize) -> Option<Never> {
    let words = from_program(words);
    let first = words.first()?;
    let prog = program(first);
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let has = |w: &str| args.contains(&w);
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    let subs: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect();

    // A shell given a command: that command.
    if matches!(prog, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh") {
        if let Some(i) = args
            .iter()
            .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
        {
            if let Some(inner) = args.get(i + 1) {
                return check(inner, at, depth + 1);
            }
        }
    }
    if prog == "eval" {
        return check(&args.join(" "), at, depth + 1);
    }

    // Permissionless, wherever it is said.
    let unasking = args.iter().any(|a| {
        let a = a.to_lowercase();
        a.starts_with("--dangerously-skip-permissions")
            || a.starts_with("--dangerously-bypass")
            || a == "--yolo"
            || a.contains("bypasspermissions")
    }) || args
        .windows(2)
        .any(|w| matches!(w[0], "--ask-for-approval" | "-a") && w[1] == "never" && prog == "codex");
    if unasking {
        return Some(Never::Permissionless);
    }

    // Secrets.
    if matches!(prog, "printenv" | "env" | "set" | "export" | "declare")
        && (prog == "printenv" || args.is_empty() || has("-p") || has("-x"))
    {
        return Some(Never::Secrets);
    }
    if matches!(prog, "source" | ".") {
        return Some(Never::Secrets)
            .filter(|_| args.iter().any(|a| secret_file(a)) || args.is_empty());
    }
    if prog == "security"
        && sub.is_some_and(|s| {
            s.starts_with("find-")
                || s.starts_with("dump-")
                || s == "export"
                || s.contains("password")
        })
    {
        return Some(Never::Secrets);
    }
    if matches!(
        prog,
        "op" | "vault" | "pass" | "aws-vault" | "doppler" | "infisical" | "chamber"
    ) || prog.contains("aws-creds")
    {
        return Some(Never::Secrets);
    }
    if prog == "gh" && subs.first() == Some(&"auth") {
        return Some(match subs.get(1) {
            Some(&"token") => Never::Secrets,
            Some(&"status") if has("-t") || has("--show-token") => Never::Secrets,
            Some(&"status") => return None,
            _ => Never::Accounts,
        });
    }
    if prog == "gcloud"
        && args
            .iter()
            .any(|a| a.contains("print-access-token") || a.contains("print-identity-token"))
    {
        return Some(Never::Secrets);
    }
    if prog == "aws"
        && subs
            .first()
            .is_some_and(|s| *s == "configure" || *s == "sts" || *s == "sso")
    {
        return Some(match subs.get(1) {
            Some(&"get" | &"export-credentials" | &"get-session-token") => Never::Secrets,
            _ if subs.first() == Some(&"sts") => Never::Secrets,
            _ => Never::Accounts,
        });
    }
    if words.iter().any(|w| secret_file(w) || secret_variable(w)) {
        return Some(Never::Secrets);
    }

    // Registries.
    let publishes = |w: &&str| w.to_lowercase().contains("publish");
    match prog {
        "cargo" if matches!(sub, Some("publish" | "yank" | "owner")) => {
            return Some(Never::Registry)
        }
        "npm" | "pnpm" | "bun" | "yarn"
            if matches!(
                sub,
                Some("publish" | "unpublish" | "deprecate" | "dist-tag" | "owner")
            ) || (prog == "yarn"
                && sub == Some("npm")
                && subs.get(1).is_some_and(publishes))
                || subs.iter().any(publishes) =>
        {
            return Some(Never::Registry)
        }
        "twine" | "flit" | "hatch" | "poetry" | "uv" | "pdm" | "maturin"
            if matches!(sub, Some("upload" | "publish")) =>
        {
            return Some(Never::Registry)
        }
        "gem" if matches!(sub, Some("push" | "yank" | "owner")) => return Some(Never::Registry),
        "docker" | "podman" | "buildah" if matches!(sub, Some("push")) => {
            return Some(Never::Registry)
        }
        "dotnet" if subs.contains(&"push") => return Some(Never::Registry),
        "mvn" | "mvnw" if subs.contains(&"deploy") => return Some(Never::Registry),
        "gradle" | "gradlew" if subs.iter().any(publishes) => return Some(Never::Registry),
        "npx" | "pnpx" | "bunx" | "lerna" | "changeset" | "semantic-release"
            if subs.iter().any(publishes) || prog == "semantic-release" =>
        {
            return Some(Never::Registry)
        }
        "make" | "just" | "task" if subs.iter().any(publishes) => return Some(Never::Registry),
        _ if (first.contains('/') || prog.ends_with(".sh")) && publishes(&prog) => {
            return Some(Never::Registry)
        }
        _ => {}
    }
    if matches!(prog, "python" | "python3" | "py")
        && args.windows(2).any(|w| w[0] == "-m" && w[1] == "twine")
    {
        return Some(Never::Registry);
    }

    // Git: history, the remote's branches and tags, new ways out, the
    // person's identity.
    if prog == "git" {
        // `git -C dir push …`: past git's own flags.
        let mut i = 0;
        while i < args.len() && args[i].starts_with('-') {
            if matches!(args[i], "-C" | "-c" | "--git-dir" | "--work-tree") {
                if args[i] == "-c"
                    && args
                        .get(i + 1)
                        .is_some_and(|v| v.to_lowercase().starts_with("credential"))
                {
                    return Some(Never::Secrets);
                }
                i += 1;
            }
            i += 1;
        }
        let git_sub = args.get(i).copied();
        let rest: Vec<&str> = args.get(i + 1..).unwrap_or_default().to_vec();
        match git_sub {
            Some("push") => {
                let forced = rest.iter().any(|a| {
                    a.starts_with("--force")
                        || *a == "-f"
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
                });
                let operands: Vec<&str> = rest
                    .iter()
                    .copied()
                    .filter(|a| !a.starts_with('-'))
                    .collect();
                if forced
                    || operands.iter().skip(1).any(|r| r.starts_with('+'))
                    || rest.contains(&"--mirror")
                {
                    return Some(Never::RewriteHistory);
                }
                let deletes = rest.iter().any(|a| {
                    matches!(*a, "--delete" | "--prune" | "-d")
                        || (a.starts_with('-') && !a.starts_with("--") && a.contains('d'))
                }) || operands.iter().skip(1).any(|r| r.starts_with(':'));
                if deletes {
                    return Some(Never::DeleteRemote);
                }
                if push_to_address(&operands) || rest.iter().any(|a| a.starts_with("--repo")) {
                    return Some(Never::WaysOut);
                }
            }
            Some("filter-branch" | "filter-repo" | "replace") => {
                return Some(Never::RewriteHistory)
            }
            Some("remote")
                if rest
                    .iter()
                    .any(|a| matches!(*a, "add" | "set-url" | "rename" | "remove" | "rm")) =>
            {
                return Some(Never::WaysOut)
            }
            Some("config") => {
                let keys = rest.iter().map(|a| a.to_lowercase());
                for k in keys {
                    if k.starts_with("credential") {
                        return Some(Never::Secrets);
                    }
                    if k.starts_with("remote.")
                        || k.starts_with("url.")
                        || k.starts_with("branch.")
                        || k.starts_with("pushurl")
                    {
                        return Some(Never::WaysOut);
                    }
                    if k.starts_with("user.") || k == "--global" || k == "--system" {
                        return Some(Never::Accounts);
                    }
                }
            }
            Some("credential" | "credential-store" | "credential-osxkeychain") => {
                return Some(Never::Secrets)
            }
            _ => {}
        }
    }

    // GitHub: what it deletes there, and money.
    if prog == "gh" {
        match subs.as_slice() {
            ["release", "delete" | "delete-asset", ..]
            | ["repo", "delete" | "archive" | "rename", ..]
            | ["ref", "delete", ..] => return Some(Never::DeleteRemote),
            ["pr", "merge", ..] if has("-d") || has("--delete-branch") => {
                return Some(Never::DeleteRemote)
            }
            ["api", ..]
                if args.windows(2).any(|w| {
                    matches!(w[0], "-X" | "--method") && w[1].eq_ignore_ascii_case("DELETE")
                }) || args.iter().any(|a| {
                    a.eq_ignore_ascii_case("-XDELETE") || a.eq_ignore_ascii_case("--method=DELETE")
                }) =>
            {
                return Some(Never::DeleteRemote)
            }
            ["secret" | "variable", ..] => return Some(Never::Secrets),
            ["extension", "install" | "upgrade" | "remove", ..] => return Some(Never::Accounts),
            _ => {}
        }
    }

    // Deleting outside its folder.
    if matches!(prog, "rm" | "rmdir" | "unlink" | "trash" | "shred" | "srm") {
        let recursive = prog != "rm"
            || args.iter().any(|a| {
                *a == "--recursive"
                    || (a.starts_with('-')
                        && !a.starts_with("--")
                        && (a.contains('r') || a.contains('R')))
            });
        let targets: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| !a.starts_with('-'))
            .collect();
        if recursive && (targets.is_empty() || targets.iter().any(|t| !deletes_inside(t, at))) {
            return Some(Never::DeleteOutside);
        }
    }
    if prog == "find" && has("-delete") {
        let roots: Vec<&str> = args
            .iter()
            .copied()
            .take_while(|a| !a.starts_with('-'))
            .collect();
        if roots.is_empty() || roots.iter().any(|t| !deletes_inside(t, at)) {
            return Some(Never::DeleteOutside);
        }
    }

    // New ways out.
    match prog {
        "scp" | "sftp" | "ftp" | "nc" | "ncat" | "netcat" | "socat" | "telnet" => {
            return Some(Never::WaysOut)
        }
        "rsync" if args.iter().any(|a| !a.starts_with('-') && a.contains(':')) => {
            return Some(Never::WaysOut)
        }
        "curl" => {
            let uploads = args.iter().any(|a| {
                a.starts_with("--data")
                    || a.starts_with("--form")
                    || a.starts_with("--upload-file")
                    || a.starts_with("--json")
                    || matches!(*a, "-d" | "-F" | "-T")
                    || (a.starts_with("-d") && a.len() > 2 && !a.starts_with("--"))
            }) || args.windows(2).any(|w| {
                matches!(w[0], "-X" | "--request")
                    && !w[1].eq_ignore_ascii_case("GET")
                    && !w[1].eq_ignore_ascii_case("HEAD")
            });
            if uploads {
                return Some(Never::WaysOut);
            }
        }
        "wget"
            if args.iter().any(|a| {
                a.starts_with("--post") || a.starts_with("--method") || a.starts_with("--body")
            }) =>
        {
            return Some(Never::WaysOut)
        }
        _ => {}
    }

    // Money.
    let pays = |w: &str| {
        let w = w.to_lowercase();
        [
            "stripe.com",
            "paypal.com",
            "braintreegateway",
            "squareup.com",
            "adyen.com",
            "/v1/charges",
            "/v1/payment",
            "/v1/payouts",
            "/v1/refunds",
            "/v1/transfers",
        ]
        .iter()
        .any(|m| w.contains(m))
    };
    if matches!(prog, "stripe" | "paypal" | "braintree") || words.iter().any(|w| pays(w)) {
        return Some(Never::Money);
    }

    // Accounts and the machine.
    let account_sub = matches!(
        sub,
        Some("login" | "logout" | "adduser" | "setup-token" | "signin" | "signout")
    );
    match prog {
        "claude" | "codex" | "npm" | "pnpm" | "yarn" | "cargo" | "docker" | "podman" | "heroku"
        | "fly" | "flyctl" | "vercel" | "netlify" | "wrangler" | "firebase" | "huggingface-cli"
            if account_sub || subs.first() == Some(&"auth") =>
        {
            return Some(Never::Accounts)
        }
        "gcloud" | "az" if subs.contains(&"login") || subs.first() == Some(&"auth") => {
            return Some(Never::Accounts)
        }
        "ssh-keyscan" | "ssh-add" | "ssh-keygen" | "ssh-copy-id" | "passwd" | "chsh" | "dscl"
        | "sysadminctl" => return Some(Never::Accounts),
        "brew" | "port" | "softwareupdate" | "apt" | "apt-get" | "dnf" | "yum" | "pacman"
            if matches!(
                sub,
                Some("uninstall" | "remove" | "rm" | "purge" | "autoremove" | "zap")
            ) =>
        {
            return Some(Never::Accounts)
        }
        "launchctl" | "systemctl"
            if matches!(
                sub,
                Some("unload" | "disable" | "bootout" | "remove" | "mask")
            ) =>
        {
            return Some(Never::Accounts)
        }
        _ => {}
    }

    // Its own limits: Desk's and the agents' settings of oversight.
    if words.iter().any(|w| {
        let w = w.to_lowercase();
        w.contains("oversight") || w.contains("workforce.toml")
    }) {
        return Some(Never::OwnLimits);
    }
    None
}

/// The hard limit `command`, a shell command a session asks to run, runs
/// into; `None` when it runs into none of them (which doesn't make it safe:
/// [`crate::computer::boss::judge`] still decides).
pub fn command(command: &str, at: &Where<'_>) -> Option<Never> {
    check(command, at, 0)
}

fn check(command: &str, at: &Where<'_>, depth: usize) -> Option<Never> {
    if depth > 4 {
        // Too deep to read: the person's, as secrets are the worst case.
        return Some(Never::Secrets);
    }
    commands(command).iter().find_map(|w| one(w, at, depth))
}

// ── Words ────────────────────────────────────────────────────────────────

/// What each limit is said as: phrases matched in lowercased text, at word
/// starts. Commands are caught by [`command`] too, which `said` reads each
/// line of the text with.
const SAID: &[(Never, &[&str])] = &[
    (
        Never::OwnLimits,
        &[
            "oversight",
            "your level",
            "trust level",
            "act fully",
            "your settings",
            "your own settings",
            "your limits",
            "your budget",
            "your permissions",
            "the boss's settings",
        ],
    ),
    (
        Never::Money,
        &[
            "make a payment",
            "take a payment",
            "send a payment",
            "pay for",
            "pay the invoice",
            "send money",
            "transfer money",
            "wire transfer",
            "buy ",
            "purchase",
            "credit card",
            "charge the card",
            "issue a refund",
            "refund the",
            "raise the budget",
            "increase the budget",
            "raise its budget",
            "release the breaker",
            "continue the breaker",
        ],
    ),
    (Never::DeleteOutside, &["rm -rf", "rm -fr", "rm -r "]),
    (
        Never::RewriteHistory,
        &[
            "force push",
            "force-push",
            "push --force",
            "push -f",
            "--force-with-lease",
            "rewrite history",
            "rewrite the history",
            "history rewrite",
            "filter-branch",
            "filter-repo",
        ],
    ),
    (
        Never::DeleteRemote,
        &[
            "delete the branch",
            "delete branch",
            "delete the remote branch",
            "delete the tag",
            "delete tag",
            "delete the release",
            "delete release",
            "delete the repo",
            "--delete-branch",
            "push --delete",
        ],
    ),
    (
        Never::Registry,
        &[
            "crates.io",
            "cargo publish",
            "npm publish",
            "yarn publish",
            "pnpm publish",
            "pypi",
            "twine",
            "gem push",
            "rubygems",
            "docker push",
            "package registry",
            "publish to npm",
            "publish the crate",
            "publish the package",
        ],
    ),
    (
        Never::Secrets,
        &[
            "aws-creds",
            ".env",
            "credentials",
            "creds",
            "api key",
            "access key",
            "secret key",
            "client secret",
            "the secrets",
            "access token",
            "auth token",
            "api token",
            "bearer token",
            "your password",
            "the password for",
            "passphrase",
            "private key",
            "keychain",
            "printenv",
            "ssh key",
        ],
    ),
    (
        Never::WaysOut,
        &[
            "git remote add",
            "remote set-url",
            "add a remote",
            "new remote",
        ],
    ),
    (
        Never::Permissionless,
        &[
            "dangerously-skip-permissions",
            "bypasspermissions",
            "--yolo",
            "skip permissions",
            "without permissions",
        ],
    ),
    (
        Never::Accounts,
        &[
            "log in to",
            "log into",
            "log out of",
            "sign in to",
            "sign out of",
            "npm login",
            "gh auth login",
            "claude login",
            "codex login",
            "docker login",
            "switch account",
            "switch accounts",
            "host key",
            "known_hosts",
            "uninstall",
        ],
    ),
    (
        Never::Records,
        &[
            "forget what",
            "forget everything",
            "wipe memory",
            "delete the memory",
            "delete the group",
            "delete the board",
            "delete the plan",
            "edit the audit",
            "always allow",
        ],
    ),
];

/// Whether `phrase` is in `text` where a word starts (`.env` in "the .env
/// file", not "the environment"; "login" not in "blogging").
fn phrase_in(text: &str, phrase: &str) -> bool {
    let mut from = 0;
    while let Some(at) = text[from..].find(phrase) {
        let i = from + at;
        let starts = i == 0
            || !text[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let ends = phrase.ends_with(' ')
            || !text[i + phrase.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || (c == '_' && !phrase.ends_with('_')));
        // A plural or a verb's ending is the same word ("secrets",
        // "logins", "refunded").
        let ends = ends
            || text[i + phrase.len()..]
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .is_some_and(|rest| matches!(rest, "s" | "es" | "ed" | "ing" | "d"));
        if starts && ends {
            return true;
        }
        from = i + phrase.len().max(1);
    }
    false
}

/// The hard limit `text`, a question a session asks or an answer to one,
/// speaks of; `None` when it speaks of none. Any line of it that reads as a
/// command is read as [`command`] reads one too, from nowhere in particular
/// (so a delete by path is caught, one relative to a folder isn't).
pub fn said(text: &str) -> Option<Never> {
    let lower = text.to_lowercase();
    if let Some((n, _)) = SAID
        .iter()
        .find(|(_, phrases)| phrases.iter().any(|p| phrase_in(&lower, p)))
    {
        return Some(*n);
    }
    let home = Path::new("/nonexistent-home");
    let nowhere = Where {
        cwd: None,
        own: None,
        home,
        scratch: &[],
    };
    // Code spans and lines, as commands; a sentence that reads as none
    // runs into nothing.
    text.split('`')
        .chain(text.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find_map(|l| {
            let found = command(l, &nowhere)?;
            // A sentence reads loosely as words: only what a command names
            // by a program counts there.
            (found != Never::DeleteOutside || l.starts_with("rm ")).then_some(found)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/sam";
    const OWN: &str = "/Users/sam/dev/acme-web";

    fn at() -> (PathBuf, PathBuf, Vec<PathBuf>) {
        (
            PathBuf::from(HOME),
            PathBuf::from(OWN),
            vec![PathBuf::from("/tmp")],
        )
    }

    fn run(c: &str) -> Option<Never> {
        let (home, own, scratch) = at();
        command(
            c,
            &Where {
                cwd: Some(&own),
                own: Some(&own),
                home: &home,
                scratch: &scratch,
            },
        )
    }

    /// One command each limit catches, and one like it that it lets be:
    /// every entry of [`ALL`] has its own, so none is a name only.
    const CASES: &[(Never, &str, &str)] = &[
        (
            Never::OwnLimits,
            "sed -i s/act/full/ ~/Desk/agents/acme/oversight.toml",
            "sed -i s/a/b/ src/lib.rs",
        ),
        (
            Never::Money,
            "curl https://api.stripe.com/v1/charges",
            "curl https://example.com/status",
        ),
        (
            Never::DeleteOutside,
            "rm -rf ../globex-api",
            "rm -rf target",
        ),
        (
            Never::RewriteHistory,
            "git push --force origin main",
            "git push origin main",
        ),
        (
            Never::DeleteRemote,
            "git push origin --delete feat/date-picker",
            "git push origin feat/date-picker",
        ),
        (
            Never::Registry,
            "cargo publish -p acme-core",
            "cargo test -p acme-core",
        ),
        (
            Never::Secrets,
            "source ~/.aws-creds && aws s3 ls",
            "source scripts/dev-env.sh",
        ),
        (
            Never::WaysOut,
            "git remote add mirror https://example.com/acme.git",
            "git remote -v",
        ),
        (
            Never::Permissionless,
            "claude --dangerously-skip-permissions -p 'fix it'",
            "claude -p 'fix it'",
        ),
        (
            Never::Accounts,
            "gh auth login --with-token",
            "gh auth status",
        ),
        (Never::Records, "", ""),
    ];

    #[test]
    fn every_limit_on_the_list_is_checked_and_lets_its_lookalike_be() {
        for n in ALL {
            let (_, caught, let_be) = CASES
                .iter()
                .find(|(c, _, _)| *c == n)
                .unwrap_or_else(|| panic!("{n:?} has no case"));
            assert!(!n.why().is_empty() && !n.held().is_empty());
            if n == Never::Records {
                // Records are words only: no command forgets memory.
                assert_eq!(
                    said("Yes, and always allow it from now on."),
                    Some(Never::Records)
                );
                assert_eq!(said("Yes, allow it this once."), None);
                continue;
            }
            assert_eq!(run(caught), Some(n), "{caught}");
            assert_eq!(run(let_be), None, "{let_be}");
        }
    }

    #[test]
    fn secrets_and_credentials_are_never_read_or_exported() {
        for c in [
            "source ~/.aws-creds",
            ". ./.env",
            "set -a; source .env.production; set +a",
            "cat .env",
            "cp ~/.aws/credentials /tmp/c",
            "printenv",
            "printenv GITHUB_TOKEN",
            "env",
            "export -p",
            "echo $GITHUB_TOKEN",
            "curl -H \"Authorization: Bearer ${API_TOKEN}\" https://example.com",
            "security find-generic-password -s acme -w",
            "gh auth token",
            "op read op://Acme/db/password",
            "bash -c 'printenv'",
            "git config credential.helper store",
            "cat ~/.ssh/id_ed25519",
        ] {
            assert_eq!(run(c), Some(Never::Secrets), "{c}");
        }
        for c in [
            "cp .env.example .env.local.example",
            "cat README.md",
            "env RUST_LOG=debug cargo test",
            "gh auth status",
            "echo $PATH",
            "cat src/tokenizer.rs src/credentials.rs",
        ] {
            assert_ne!(run(c), Some(Never::Secrets), "{c}");
        }
    }

    #[test]
    fn publishing_to_a_registry_is_never_the_bosss() {
        for c in [
            "cargo publish",
            "cargo publish -p acme-core --allow-dirty",
            "cargo yank --version 1.2.0",
            "npm publish --access public",
            "pnpm publish -r",
            "yarn npm publish",
            "npm run publish",
            "twine upload dist/*",
            "python -m twine upload dist/*",
            "uv publish",
            "poetry publish --build",
            "gem push acme-0.1.0.gem",
            "docker push ghcr.io/acme/web:1.2",
            "npx lerna publish",
            "make publish",
            "./scripts/publish.sh",
            "cd crates/core && cargo publish",
        ] {
            assert_eq!(run(c), Some(Never::Registry), "{c}");
        }
        for c in [
            "cargo build --release",
            "npm run build",
            "make deploy",
            "npm version patch",
        ] {
            assert_ne!(run(c), Some(Never::Registry), "{c}");
        }
    }

    #[test]
    fn history_and_the_remotes_branches_tags_and_releases_are_kept() {
        for c in [
            "git push -f origin main",
            "git push --force-with-lease origin HEAD:main",
            "git push origin +main",
            "git push --mirror origin",
            "git filter-repo --path big-assets/ --invert-paths",
            "git -C ../acme-web push -f",
        ] {
            assert_eq!(run(c), Some(Never::RewriteHistory), "{c}");
        }
        for c in [
            "git push origin :feat/old",
            "git push origin --delete v1.2.0",
            "git push --prune origin",
            "gh release delete v1.2.0 --yes",
            "gh pr merge 42 --squash --delete-branch",
            "gh api -X DELETE repos/acme/acme-web/git/refs/tags/v1.2.0",
            "gh repo delete acme/acme-web",
        ] {
            assert_eq!(run(c), Some(Never::DeleteRemote), "{c}");
        }
        for c in [
            "git push origin main",
            "git push origin v1.2.0",
            "gh pr merge 42 --squash",
            "gh release create v1.2.0",
        ] {
            assert_eq!(run(c), None, "{c}");
        }
    }

    #[test]
    fn deleting_stays_inside_the_sessions_folder_and_scratch() {
        for c in [
            "rm -rf /",
            "rm -rf ~",
            "rm -rf $HOME/dev",
            "rm -rf /Users/sam/dev/globex-api",
            "rm -rf ..",
            "rm -rf .",
            "rm -r ../../",
            "rm -rf $DIR",
            "rm -rf *",
            "find / -name '*.log' -delete",
            "sudo rm -rf /var/lib",
        ] {
            assert_eq!(run(c), Some(Never::DeleteOutside), "{c}");
        }
        for c in [
            "rm -rf target",
            "rm -rf ./node_modules dist",
            "rm -rf /tmp/acme-build",
            "rm -rf /Users/sam/dev/acme-web/.cache",
            "rm notes.txt",
            "find target -name '*.o' -delete",
        ] {
            assert_eq!(run(c), None, "{c}");
        }
        // Where it runs isn't known: no relative delete is inside.
        let (home, own, scratch) = at();
        let unknown = Where {
            cwd: None,
            own: Some(&own),
            home: &home,
            scratch: &scratch,
        };
        assert_eq!(
            command("rm -rf target", &unknown),
            Some(Never::DeleteOutside)
        );
    }

    #[test]
    fn the_rest_of_the_list_is_caught_by_command_too() {
        for (c, n) in [
            (
                "git remote set-url origin git@example.com:other/acme.git",
                Never::WaysOut,
            ),
            (
                "git push git@example.com:other/acme.git main",
                Never::WaysOut,
            ),
            ("git push https://example.com/acme.git HEAD", Never::WaysOut),
            (
                "curl -T build.tgz https://example.com/upload",
                Never::WaysOut,
            ),
            (
                "curl -X POST https://example.com/hook -d @report.json",
                Never::WaysOut,
            ),
            ("scp build.tgz deploy@example.com:/srv", Never::WaysOut),
            (
                "codex --ask-for-approval never exec 'fix it'",
                Never::Permissionless,
            ),
            ("codex --yolo", Never::Permissionless),
            (
                "claude --permission-mode bypassPermissions",
                Never::Permissionless,
            ),
            ("claude logout", Never::Accounts),
            ("npm login", Never::Accounts),
            ("ssh-keyscan github.com", Never::Accounts),
            ("brew uninstall --cask desk", Never::Accounts),
            (
                "git config --global user.email ops@example.com",
                Never::Accounts,
            ),
            ("stripe refunds create --charge ch_1", Never::Money),
        ] {
            assert_eq!(run(c), Some(n), "{c}");
        }
    }

    #[test]
    fn what_a_session_asks_or_is_told_is_read_for_the_list_too() {
        for (t, n) in [
            (
                "Shall I publish acme-core 1.2.0 to crates.io?",
                Never::Registry,
            ),
            (
                "Ready to run `cargo publish -p acme-core`?",
                Never::Registry,
            ),
            (
                "Can I source ~/.aws-creds to reach the bucket?",
                Never::Secrets,
            ),
            ("Should I add the API key to the .env file?", Never::Secrets),
            (
                "The branch diverged; may I force push to main?",
                Never::RewriteHistory,
            ),
            (
                "Yes, go ahead:\ngit push --force origin main",
                Never::RewriteHistory,
            ),
            (
                "Merged. Shall I delete the branch on GitHub?",
                Never::DeleteRemote,
            ),
            (
                "Should I raise the budget so it can keep going?",
                Never::Money,
            ),
            (
                "Shall I buy the Pro plan for the test account?",
                Never::Money,
            ),
            ("May I switch your level to act fully?", Never::OwnLimits),
            ("It needs you to log in to npm first.", Never::Accounts),
        ] {
            assert_eq!(said(t), Some(n), "{t}");
        }
        for t in [
            "Tests pass on feat/date-picker. Shall I merge PR 42 and tag v1.2.0?",
            "Ready to promote build 318 to staging?",
            "Deploy acme-web to production now?",
            "Yes: push to main and deploy.",
            "How many tokens did that session use?",
            "The environment is ready; shall I run the migrations' tests?",
            "I'm blogging the release notes in CHANGELOG.md.",
            "The login form's tests fail; shall I fix the password reset too?",
            "The payment service's tests pass. Merge it?",
            "cat src/tokenizer.rs",
        ] {
            assert_eq!(said(t), None, "{t}");
        }
    }
}
