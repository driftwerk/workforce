//! What a coding round that failed tells the agent: which engine it was,
//! why in plain words (not installed, not signed in, at its limit, or how
//! it exited and the last lines it wrote, secrets hidden), and what fixes it.

use eyre::Result;

use super::CliRoundResult;
use crate::hub_core::logins::limits::{
    codex_limit_reached, is_rate_limited, is_usage_capped, looks_like_auth_failure,
};

/// An engine as a person knows it: its name, the CLI it runs, how that is
/// installed, and how it is signed in.
struct Named {
    name: &'static str,
    cli: &'static str,
    install: &'static str,
    sign_in: &'static str,
}

fn named(label: &str) -> Named {
    const CLAUDE: Named = Named {
        name: "Claude Code",
        cli: "claude",
        install: "npm install -g @anthropic-ai/claude-code",
        sign_in:
            "run `claude` in a terminal and sign in, or connect a Claude plan in Desk's Accounts",
    };
    match label {
        super::codex::LABEL => Named {
            name: "Codex",
            cli: "codex",
            install: "npm install -g @openai/codex",
            sign_in:
                "run `codex login` in a terminal, or sign in to a ChatGPT plan in Desk's Accounts",
        },
        super::codex::API_LABEL => Named {
            name: "Codex (on an OpenAI API key)",
            cli: "codex",
            install: "npm install -g @openai/codex",
            sign_in: "set a valid OpenAI API key in the vault (`openai/api_key`)",
        },
        super::bedrock::LABEL => Named {
            name: "Claude Code (on Bedrock)",
            sign_in: "check the role in `[coding_session.bedrock]` can call Bedrock",
            ..CLAUDE
        },
        _ => CLAUDE,
    }
}

/// The most of a CLI's stderr a failure quotes.
const TAIL_LINES: usize = 5;
const TAIL_LINE_CHARS: usize = 300;

/// The last lines of `stderr`, secrets hidden first so no cut leaves part
/// of one a pattern no longer finds.
fn last_lines(stderr: &str) -> String {
    let redacted = crate::redact::hide_secrets(stderr);
    let lines: Vec<&str> = redacted
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..]
        .iter()
        .map(|l| match l.char_indices().nth(TAIL_LINE_CHARS) {
            Some((at, _)) => format!("{}…", &l[..at]),
            None => (*l).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the CLI says it has no sign-in, or that its sign-in was refused.
fn signed_out(text: &str) -> bool {
    let lower = text.to_lowercase();
    looks_like_auth_failure(text)
        || lower.contains("not logged in")
        || lower.contains("not signed in")
        || lower.contains("please run /login")
        || lower.contains("codex login")
        || lower.contains("no auth")
}

/// Whether the CLI could not be found or run at all.
fn missing(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("failed to spawn")
        && (lower.contains("no such file")
            || lower.contains("not found")
            || lower.contains("os error 2"))
}

fn limited(text: &str) -> bool {
    is_rate_limited(text) || is_usage_capped(text) || codex_limit_reached(text)
}

fn not_installed(e: &Named) -> String {
    format!(
        "{} isn't installed on this computer: `{}` wasn't found. Install it with `{}`, then try again.",
        e.name, e.cli, e.install
    )
}

fn not_signed_in(e: &Named) -> String {
    format!(
        "{} isn't signed in, or its sign-in has expired. To fix it, {}.",
        e.name, e.sign_in
    )
}

fn at_limit(e: &Named) -> String {
    format!(
        "{} is at its plan's usage or rate limit. Try again once it resets; it doesn't fall back to a paid API.",
        e.name
    )
}

/// What a failed round on the engine `label` names says, for the agent to
/// relay: never a secret, never another engine's name.
pub fn round_failure(label: &str, result: &Result<CliRoundResult>) -> String {
    let e = named(label);
    match result {
        Err(error) => {
            let said = format!("{error:#}");
            if missing(&said) {
                not_installed(&e)
            } else if limited(&said) {
                at_limit(&e)
            } else if signed_out(&said) {
                not_signed_in(&e)
            } else {
                format!(
                    "{} couldn't run: {}. Check that `{} --version` works in a terminal on this computer.",
                    e.name,
                    last_lines(&said),
                    e.cli
                )
            }
        }
        Ok(r) => {
            let said = format!("{}\n{}\n{}", r.notice, r.text, r.stderr_tail);
            if limited(&said) {
                return at_limit(&e);
            }
            if signed_out(&r.notice) || signed_out(&r.stderr_tail) || signed_out(&r.text) {
                return not_signed_in(&e);
            }
            let how = r
                .crash_kind
                .as_ref()
                .map(|k| k.label())
                .unwrap_or_else(|| format!("exit {}", r.exit_code));
            let what = if r.num_turns == 0 {
                "failed to start"
            } else {
                "stopped with an error"
            };
            let tail = last_lines(&r.stderr_tail);
            let tail = if tail.is_empty() {
                last_lines(&r.text)
            } else {
                tail
            };
            let mut text = format!("{} {what} ({how}).", e.name);
            if !tail.is_empty() {
                text.push_str(&format!(" The last it said:\n\n{tail}\n\n"));
            } else {
                text.push(' ');
            }
            text.push_str(&format!(
                "Check that `{} --version` works in a terminal on this computer and that it's signed in ({}). It doesn't fall back to a paid API.",
                e.cli, e.sign_in
            ));
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: i32, stderr: &str) -> Result<CliRoundResult> {
        Ok(CliRoundResult {
            exit_code: code,
            stderr_tail: stderr.to_string(),
            completion: crate::claude_code::Completion::Failed,
            ..CliRoundResult::default()
        })
    }

    #[test]
    fn a_missing_cli_is_named_with_how_to_install_it() {
        let gone = Err(eyre::eyre!(
            "Failed to spawn codex CLI (is it installed in the image?): No such file or directory (os error 2)"
        ));
        let text = round_failure(super::super::codex::LABEL, &gone);
        assert_eq!(
            text,
            "Codex isn't installed on this computer: `codex` wasn't found. Install it with `npm install -g @openai/codex`, then try again."
        );
        let text = round_failure(super::super::claude_code::LABEL, &Err(eyre::eyre!(
            "Failed to spawn claude CLI (is it installed in the image?): No such file or directory (os error 2)"
        )));
        assert!(text.starts_with("Claude Code isn't installed"), "{text}");
        assert!(text.contains("@anthropic-ai/claude-code"), "{text}");
    }

    #[test]
    fn a_cli_that_isnt_signed_in_says_how_to_sign_in() {
        let text = round_failure(
            super::super::codex::LABEL,
            &exited(1, "Error: Not logged in. Please run `codex login`."),
        );
        assert!(text.starts_with("Codex isn't signed in"), "{text}");
        assert!(
            text.contains("codex login") && !text.contains("Claude"),
            "{text}"
        );
        let text = round_failure(
            super::super::claude_code::LABEL,
            &exited(1, "Invalid API key · Please run /login"),
        );
        assert!(text.starts_with("Claude Code isn't signed in"), "{text}");
    }

    #[test]
    fn a_limit_is_said_as_one() {
        let text = round_failure(
            super::super::codex::LABEL,
            &exited(1, "ERROR: You've hit your usage limit. Try again later."),
        );
        assert!(
            text.starts_with("Codex is at its plan's usage or rate limit"),
            "{text}"
        );
    }

    #[test]
    fn a_crash_says_its_exit_and_last_lines_with_secrets_hidden() {
        let key = format!("sk-ant-api03-{}", "A1b2C3d4E5".repeat(9));
        let stderr = format!(
            "loading config\nline 2\nline 3\nline 4\nusing key {key}\nfatal: could not read the workspace"
        );
        let text = round_failure(super::super::claude_code::LABEL, &exited(1, &stderr));
        assert!(
            text.starts_with("Claude Code failed to start (exit 1). The last it said:"),
            "{text}"
        );
        assert!(
            text.contains("fatal: could not read the workspace"),
            "{text}"
        );
        assert!(
            !text.contains("loading config"),
            "only its last lines: {text}"
        );
        assert!(
            !text.contains(&key) && !text.contains("A1b2C3d4E5"),
            "{text}"
        );
        assert!(text.contains("REDACTED"), "{text}");
        assert!(text.contains("`claude --version`"), "{text}");
        // A round that ran turns stopped, rather than failed to start.
        let ran = Ok(CliRoundResult {
            num_turns: 3,
            ..exited(2, "boom").unwrap()
        });
        let text = round_failure(super::super::codex::LABEL, &ran);
        assert!(
            text.starts_with("Codex stopped with an error (exit 2)."),
            "{text}"
        );
    }
}
