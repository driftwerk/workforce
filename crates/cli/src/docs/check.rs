//! Mechanical checks on a docs page's examples. Every `workforce …` command
//! in a shell block must parse with the real command line, and every
//! `workforce.toml` block must load into the real config with no key the
//! config doesn't have. Prose can't be checked this way; examples can, and
//! an example that doesn't parse is wrong however well it reads.

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, Parser};
use workforce_common::settings::WorkforceConfig;

/// A fenced code block on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub lang: String,
    /// The `title="…"` from the fence's meta, which names the file a config
    /// block belongs to.
    pub title: Option<String>,
    pub body: String,
    /// 1-based line of the opening fence.
    pub line: usize,
}

/// Which file a page's untitled TOML blocks show. A block titled with a file
/// name is that file whatever the page says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TomlSchema {
    /// `workforce.toml`, checked against [`WorkforceConfig`].
    Workforce,
    /// A file with no checker; its blocks are counted as unchecked.
    Unchecked,
}

/// One example that fails its check.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Finding {
    /// 1-based line on the page.
    pub line: usize,
    /// The example as written (a command, or a block's first line).
    pub example: String,
    pub problem: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: `{}` — {}",
            self.line, self.example, self.problem
        )
    }
}

/// What checking a page found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PageCheck {
    pub findings: Vec<Finding>,
    pub commands_checked: usize,
    pub config_blocks_checked: usize,
    /// TOML blocks for files with no checker.
    pub config_blocks_unchecked: usize,
}

/// Check every example on a page.
pub fn check_page(page: &str, untitled_toml: TomlSchema) -> PageCheck {
    let mut report = PageCheck {
        findings: fence_problems(page),
        ..PageCheck::default()
    };
    for block in code_blocks(page) {
        match block.lang.as_str() {
            "sh" | "bash" | "shell" | "console" | "zsh" => {
                for command in workforce_commands(&block) {
                    report.commands_checked += 1;
                    match command.argv {
                        Ok(argv) => {
                            if let Err(problem) = check_cli(&argv) {
                                report.findings.push(Finding {
                                    line: command.line,
                                    example: command.text,
                                    problem,
                                });
                            }
                        }
                        Err(problem) => report.findings.push(Finding {
                            line: command.line,
                            example: command.text,
                            problem,
                        }),
                    }
                }
            }
            "toml" => {
                let schema = match block.title.as_deref() {
                    Some(title) if file_name(title) == "workforce.toml" => TomlSchema::Workforce,
                    Some(_) => TomlSchema::Unchecked,
                    None => untitled_toml,
                };
                match schema {
                    TomlSchema::Unchecked => report.config_blocks_unchecked += 1,
                    TomlSchema::Workforce => {
                        report.config_blocks_checked += 1;
                        let example = block.body.lines().next().unwrap_or("").trim().to_string();
                        for problem in check_workforce_toml(&block.body) {
                            report.findings.push(Finding {
                                line: block.line,
                                example: example.clone(),
                                problem,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    report.findings.sort();
    report
}

fn file_name(title: &str) -> &str {
    title.rsplit(['/', '\\']).next().unwrap_or(title)
}

// ─── Code blocks ───────────────────────────────────────────────────────────

/// Every fenced code block on the page. A fence is three or more backticks
/// or tildes, closed by a run of the same character at least as long.
pub fn code_blocks(page: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(char, usize, String, Option<String>, usize, Vec<&str>)> = None;
    for (index, line) in page.lines().enumerate() {
        let trimmed = line.trim_start();
        let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let fence_len = fence_char.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        match &mut open {
            None => {
                if fence_len >= 3 {
                    let info = trimmed[fence_len..].trim();
                    let (lang, meta) = info.split_once(char::is_whitespace).unwrap_or((info, ""));
                    open = Some((
                        fence_char.unwrap(),
                        fence_len,
                        lang.to_lowercase(),
                        meta_title(meta),
                        index + 1,
                        Vec::new(),
                    ));
                }
            }
            Some((ch, len, _, _, _, body)) => {
                let closes = fence_char == Some(*ch)
                    && fence_len >= *len
                    && trimmed[fence_len..].trim().is_empty();
                if closes {
                    let (_, _, lang, title, start, body) = open.take().unwrap();
                    blocks.push(CodeBlock {
                        lang,
                        title,
                        body: body.join("\n"),
                        line: start,
                    });
                } else {
                    body.push(line);
                }
            }
        }
    }
    blocks
}

/// Code fences that leave the page's structure broken: a block that never
/// closes (the rest of the page renders as code), a fence joined onto the
/// end of a line (so it closes nothing), a fence with a language opening
/// inside another block (the usual sign the block's closing fence went
/// missing), and an empty block. None of these fail the docs build.
pub fn fence_problems(page: &str) -> Vec<Finding> {
    let mut problems = Vec::new();
    let mut open: Option<(char, usize, usize, bool)> = None; // char, len, line, has content
    let finding = |line: usize, text: &str, problem: &str| Finding {
        line,
        example: text.trim().chars().take(80).collect(),
        problem: problem.to_string(),
    };
    for (index, line) in page.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let fence_len = fence_char.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        let info = if fence_len >= 3 {
            trimmed[fence_len..].trim()
        } else {
            ""
        };
        match &mut open {
            None => {
                if fence_len >= 3 {
                    open = Some((fence_char.unwrap(), fence_len, number, false));
                }
            }
            Some((ch, len, start, has_content)) => {
                if fence_char == Some(*ch) && fence_len >= *len {
                    if info.is_empty() {
                        if !*has_content {
                            problems.push(finding(*start, "```", "empty code block"));
                        }
                        open = None;
                    } else {
                        problems.push(finding(
                            number,
                            line,
                            &format!(
                                "a code fence with a language opens inside the code block from \
                                 line {start}; is that block's closing fence missing or joined \
                                 to a line?"
                            ),
                        ));
                    }
                    continue;
                }
                let end = line.trim_end();
                if end.ends_with(&ch.to_string().repeat(3)) && fence_len < 3 {
                    problems.push(finding(
                        number,
                        line,
                        "a code fence is joined to the end of this line, so it closes nothing; \
                         put it on a line of its own",
                    ));
                }
                if !line.trim().is_empty() {
                    *has_content = true;
                }
            }
        }
    }
    if let Some((_, _, start, _)) = open {
        problems.push(finding(
            start,
            "```",
            "this code block never closes, so the rest of the page renders as code",
        ));
    }
    problems
}

fn meta_title(meta: &str) -> Option<String> {
    let rest = &meta[meta.find("title=")? + "title=".len()..];
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let inner = &rest[1..];
    Some(inner[..inner.find(quote)?].to_string())
}

// ─── Commands ──────────────────────────────────────────────────────────────

/// A `workforce …` command found in a shell block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub line: usize,
    pub text: String,
    /// The arguments, or why the line couldn't be split into them.
    pub argv: Result<Vec<String>, String>,
}

/// The `workforce` commands in a shell block: one per logical line (a
/// trailing `\` continues it), one per pipeline stage or `&&`/`;` part, with
/// `$ ` prompts, leading `VAR=value` assignments, comments, redirections and
/// `[…]` optional-argument brackets set aside, and a `$(…)` substitution
/// standing for one value. Other programs are skipped.
pub fn workforce_commands(block: &CodeBlock) -> Vec<Command> {
    let mut commands = Vec::new();
    let mut logical = String::new();
    let mut start = 0;
    for (offset, raw) in block.body.lines().enumerate() {
        let line = raw.trim();
        if logical.is_empty() {
            start = block.line + 1 + offset;
        }
        if let Some(cont) = line.strip_suffix('\\') {
            logical.push_str(cont);
            logical.push(' ');
            continue;
        }
        logical.push_str(line);
        let text = std::mem::take(&mut logical);
        let text = text.trim().strip_prefix("$ ").unwrap_or(text.trim()).trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let text = substitutions_as_values(text);
        let text = text.as_str();
        let Some(tokens) = shlex::split(text) else {
            if mentions_workforce(text) {
                commands.push(Command {
                    line: start,
                    text: text.to_string(),
                    argv: Err("unbalanced quotes".to_string()),
                });
            }
            continue;
        };
        let tokens: Vec<String> = tokens
            .into_iter()
            .take_while(|t| !t.starts_with('#'))
            .collect();
        for part in tokens.split(|t| matches!(t.as_str(), "|" | "||" | "&&" | ";" | "&")) {
            if let Some(argv) = workforce_argv(part) {
                commands.push(Command {
                    line: start,
                    text: argv.join(" "),
                    argv: Ok(argv),
                });
            }
        }
    }
    commands
}

fn mentions_workforce(text: &str) -> bool {
    text.split_whitespace().any(is_workforce)
}

fn is_workforce(token: &str) -> bool {
    token == "workforce" || token.ends_with("/workforce")
}

fn workforce_argv(part: &[String]) -> Option<Vec<String>> {
    let program = part.iter().position(|t| {
        !is_assignment(t) && !matches!(t.as_str(), "sudo" | "exec" | "time" | "env")
    })?;
    if !is_workforce(&part[program]) {
        return None;
    }
    let mut argv = vec!["workforce".to_string()];
    let mut rest = part[program + 1..].iter();
    while let Some(token) = rest.next() {
        // `[--source <name>]`: usage syntax for an optional argument.
        if token.starts_with('[') {
            let mut end = token.clone();
            while !end.ends_with(']') {
                match rest.next() {
                    Some(next) => end = next.clone(),
                    None => break,
                }
            }
            continue;
        }
        match token.as_str() {
            ">" | ">>" | "<" | "2>" | "2>>" | "&>" => {
                rest.next();
            }
            "2>&1" | "..." | "…" => {}
            t if t.starts_with('>') || t.starts_with("2>") => {}
            _ => argv.push(token.clone()),
        }
    }
    Some(argv)
}

/// Replace each `$(…)` command substitution with a single placeholder
/// value, so it splits as the one argument it produces.
fn substitutions_as_values(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'(') {
            chars.next();
            let mut depth = 1;
            for inner in chars.by_ref() {
                match inner {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push_str("<substituted>");
        } else {
            out.push(c);
        }
    }
    out
}

fn is_assignment(token: &str) -> bool {
    token.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Parse `argv` with the real command line. Help and version requests
/// pass, and so does a value written as a placeholder (`<path>`) for an
/// argument that only takes certain values, and a required argument left out
/// that an environment variable can supply (`--token`,
/// `WORKFORCE_GATEWAY_TOKEN`).
pub fn check_cli(argv: &[String]) -> Result<(), String> {
    let Err(err) = crate::Cli::try_parse_from(argv) else {
        return Ok(());
    };
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => Ok(()),
        ErrorKind::InvalidValue | ErrorKind::ValueValidation
            if matches!(
                err.get(ContextKind::InvalidValue),
                Some(ContextValue::String(v)) if is_placeholder(v)
            ) =>
        {
            Ok(())
        }
        ErrorKind::MissingRequiredArgument => match err.get(ContextKind::InvalidArg) {
            Some(ContextValue::Strings(missing)) if all_from_env(argv, missing) => Ok(()),
            Some(ContextValue::Strings(missing)) => {
                Err(format!("missing required {}", missing.join(", ")))
            }
            _ => Err(first_line(&err.render().to_string())),
        },
        _ => Err(first_line(&err.render().to_string())),
    }
}

/// Whether every missing argument (as clap names it, `--token <TOKEN>`) of
/// the subcommand `argv` reaches can come from an environment variable.
fn all_from_env(argv: &[String], missing: &[String]) -> bool {
    let mut command = crate::Cli::command();
    for token in argv.iter().skip(1) {
        if let Some(sub) = command.find_subcommand(token).cloned() {
            command = sub;
        }
    }
    missing.iter().all(|name| {
        let long = name
            .split_whitespace()
            .next()
            .and_then(|flag| flag.strip_prefix("--"));
        long.is_some_and(|long| {
            command
                .get_arguments()
                .any(|arg| arg.get_long() == Some(long) && arg.get_env().is_some())
        })
    })
}

fn is_placeholder(value: &str) -> bool {
    (value.starts_with('<') && value.ends_with('>'))
        || (value.starts_with('{') && value.ends_with('}'))
        || (value.starts_with('[') && value.ends_with(']'))
        || value
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c == '-')
}

fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .trim_start_matches("error: ")
        .to_string()
}

// ─── Config ────────────────────────────────────────────────────────────────

/// Problems loading a `workforce.toml` example into [`WorkforceConfig`]: not
/// TOML, a value of the wrong type, or a key the config doesn't have. A line
/// that is just `...` stands for omitted lines and is skipped.
pub fn check_workforce_toml(body: &str) -> Vec<String> {
    let body: String = body
        .lines()
        .filter(|l| !matches!(l.trim(), "..." | "…" | "# ..." | "# …"))
        .collect::<Vec<_>>()
        .join("\n");
    let table: toml::Table = match toml::from_str(&body) {
        Ok(table) => table,
        Err(e) => return vec![format!("not valid TOML: {}", first_line(&e.to_string()))],
    };
    let mut unknown = Vec::new();
    let loaded: Result<WorkforceConfig, toml::de::Error> =
        serde_ignored::deserialize(toml::Value::Table(table), |path| {
            unknown.push(path.to_string())
        });
    match loaded {
        Err(e) => vec![format!("doesn't load: {}", first_line(&e.to_string()))],
        Ok(_) => unknown
            .into_iter()
            .map(|path| format!("`{path}` is not a workforce.toml key"))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &str) -> Vec<String> {
        shlex::split(command).unwrap()
    }

    fn fails(command: &str) -> String {
        check_cli(&argv(command)).expect_err(command)
    }

    #[test]
    fn todays_wrong_cli_examples_fail() {
        // From the review of the curator's cli.mdx draft (PR #21).
        assert!(fails("workforce decide export-states --state_chars 2000").contains("state_chars"));
        fails("workforce shadow promote --store memories --config workforce.toml");
        fails("workforce playbook create --id incident-response");
        fails("workforce permissions import-roles --config workforce.toml --apply");
        fails("workforce memory relationship-scan");
    }

    #[test]
    fn their_corrected_forms_pass() {
        for command in [
            "workforce shadow promote memories --config workforce.toml",
            "workforce playbook create incident-response",
            "workforce permissions import-roles --config workforce.toml --from-csv roles.csv --apply",
            "workforce memory relationship-scan --since 30d",
            "workforce --help",
            "workforce vault",
        ] {
            check_cli(&argv(command)).unwrap_or_else(|e| panic!("{command}: {e}"));
        }
    }

    #[test]
    fn a_missing_argument_is_named() {
        assert_eq!(
            fails("workforce memory backfill"),
            "missing required --since <SINCE>, --agent <AGENT>"
        );
    }

    #[test]
    fn an_argument_the_environment_supplies_may_be_left_out() {
        // `--url`/`--token` fall back to WORKFORCE_GATEWAY_URL/_TOKEN.
        check_cli(&argv("workforce eval restore --out ./runs/know-how")).unwrap();
        // A positional has no such fallback.
        assert!(fails("workforce heartbeat enable").contains("<NAME>"));
    }

    #[test]
    fn substitutions_and_optional_brackets_are_read_as_written() {
        let page = "```sh\nworkforce chat --gateway ws://h:8091 --token $(grep auth_token workforce.toml | cut -d= -f2)\nworkforce skill catalog [--source <name>]\n```\n";
        let commands = workforce_commands(&code_blocks(page)[0]);
        assert_eq!(commands.len(), 2);
        for command in commands {
            check_cli(command.argv.as_ref().unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", command.text));
        }
    }

    #[test]
    fn a_placeholder_value_is_not_a_wrong_value() {
        check_cli(&argv(
            "workforce memory relationship-scan --since <duration>",
        ))
        .unwrap();
    }

    #[test]
    fn todays_wrong_config_examples_fail() {
        // From the reviews of PRs #24 and #25.
        let shared = check_workforce_toml("[shared_channels]\nchannel = \"C123\"\n");
        assert!(shared[0].starts_with("doesn't load"), "{shared:?}");
        let tiers = check_workforce_toml(
            "[[tiers]]\nname = \"cheap\"\nproviders = [{ provider = \"anthropic\", model = \"x\" }]\n",
        );
        assert_eq!(
            tiers,
            vec!["`tiers` is not a workforce.toml key".to_string()]
        );
        let typo = check_workforce_toml("[curator]\nmax_token = 8000\n");
        assert_eq!(
            typo,
            vec!["`curator.max_token` is not a workforce.toml key".to_string()]
        );
    }

    #[test]
    fn a_real_config_example_passes() {
        let ok = check_workforce_toml(
            "[curator]\ntier = \"openrouter:deepseek:powerful\"\nmax_tokens = 8000\n...\n",
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn commands_are_found_across_prompts_continuations_and_pipes() {
        let page = "Intro\n\n```sh\n$ workforce agent list \\\n    --config workforce.toml\n# a comment\nWORKFORCE_X=1 workforce sessions --history | head -5\ncurl -s localhost | jq .\nworkforce status > out.txt && echo done\n```\n";
        let block = &code_blocks(page)[0];
        let found: Vec<(usize, String)> = workforce_commands(block)
            .into_iter()
            .map(|c| (c.line, c.text))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    4,
                    "workforce agent list --config workforce.toml".to_string()
                ),
                (7, "workforce sessions --history".to_string()),
                (9, "workforce status".to_string()),
            ]
        );
    }

    #[test]
    fn code_blocks_carry_their_language_title_and_line() {
        let page = "a\n````mdx\n```toml\n[x]\n```\n````\n\n```toml title=\"tiers.toml\"\n[anthropic]\n```\n";
        let blocks = code_blocks(page);
        assert_eq!(blocks.len(), 2, "a fence inside a longer fence is content");
        assert_eq!(blocks[0].lang, "mdx");
        assert_eq!(blocks[1].title.as_deref(), Some("tiers.toml"));
        assert_eq!(blocks[1].line, 8);
        assert_eq!(blocks[1].body, "[anthropic]");
    }

    #[test]
    fn a_fence_joined_to_a_line_is_found() {
        // What the curator wrote in the final acceptance run.
        let page = "## Memory\n\n```sh\nworkforce memory backfill --since 30d --agent default```\n\n## Lessons\n\n```sh\nworkforce lessons list\n```\n";
        let problems: Vec<String> = fence_problems(page)
            .into_iter()
            .map(|f| f.problem)
            .collect();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("joined to the end of this line")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("opens inside the code block")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unclosed_and_an_empty_block_are_found() {
        let unclosed = fence_problems("intro\n\n```sh\nworkforce status\n");
        assert_eq!(unclosed.len(), 1);
        assert!(unclosed[0].problem.contains("never closes"));
        assert_eq!(unclosed[0].line, 3);
        let empty = fence_problems("```text\n```\n");
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].problem, "empty code block");
    }

    #[test]
    fn well_formed_fences_have_no_problems() {
        let page = "````mdx\n```sh\nworkforce status\n```\n````\n\n~~~toml\n[x]\n~~~\n";
        assert!(
            fence_problems(page).is_empty(),
            "{:?}",
            fence_problems(page)
        );
    }

    #[test]
    fn a_titled_block_is_checked_as_the_file_it_names() {
        let page = "```toml title=\"tiers.toml\"\n[anthropic]\ncheap = \"x\"\n```\n\n```toml\n[[tiers]]\n```\n";
        let report = check_page(page, TomlSchema::Workforce);
        assert_eq!(report.config_blocks_unchecked, 1);
        assert_eq!(report.config_blocks_checked, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].line, 6);
    }
}
