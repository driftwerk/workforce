//! Redact known secret formats from any text destined for an external sink
//! (Slack, agent prompts carrying prior step output, webhook deliveries).
//!
//! Deny-by-default: a false positive (replacing a non-secret that matched a
//! pattern) is acceptable; a false negative (leaking a real token) is not.
//! Applied at the boundary where text crosses into an external surface, not
//! at generation time — generating code is trusted, the concern is exfil.
//!
//! Patterns are ordered most-specific first so a `ghp_…` match is labelled
//! as a GitHub PAT rather than caught by a generic bearer-token sweep.

use regex::Regex;
use std::sync::OnceLock;

struct Pattern {
    regex: Regex,
    label: &'static str,
}

fn patterns() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            Pattern {
                regex: Regex::new(r"ghp_[A-Za-z0-9]{36,}").unwrap(),
                label: "GITHUB_PAT",
            },
            Pattern {
                regex: Regex::new(r"github_pat_[A-Za-z0-9_]{80,}").unwrap(),
                label: "GITHUB_PAT_FINE_GRAINED",
            },
            Pattern {
                regex: Regex::new(r"ghs_[A-Za-z0-9]{36,}").unwrap(),
                label: "GITHUB_SERVER_TOKEN",
            },
            Pattern {
                regex: Regex::new(r"gho_[A-Za-z0-9]{36,}").unwrap(),
                label: "GITHUB_OAUTH_TOKEN",
            },
            Pattern {
                regex: Regex::new(r"sk-ant-[A-Za-z0-9_-]{40,}").unwrap(),
                label: "ANTHROPIC_KEY",
            },
            Pattern {
                regex: Regex::new(r"sk-proj-[A-Za-z0-9_-]{40,}").unwrap(),
                label: "OPENAI_PROJECT_KEY",
            },
            Pattern {
                regex: Regex::new(r"sk-[A-Za-z0-9]{48}").unwrap(),
                label: "OPENAI_KEY",
            },
            Pattern {
                regex: Regex::new(r"glpat-[A-Za-z0-9_-]{20,}").unwrap(),
                label: "GITLAB_PAT",
            },
            Pattern {
                regex: Regex::new(r"xox[baprs]-[A-Za-z0-9-]{10,}").unwrap(),
                label: "SLACK_TOKEN",
            },
            Pattern {
                regex: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                label: "AWS_ACCESS_KEY",
            },
            Pattern {
                regex: Regex::new(r"AIza[A-Za-z0-9_-]{35}").unwrap(),
                label: "GOOGLE_API_KEY",
            },
            // Generic bearer/Authorization headers — must come AFTER the specific
            // patterns so labelled tokens win. Requires at least 20 chars after
            // the scheme so random words don't trigger it.
            Pattern {
                regex: Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{20,}").unwrap(),
                label: "BEARER_TOKEN",
            },
            Pattern {
                regex: Regex::new(r"(?i)authorization:\s*[A-Za-z]+\s+[A-Za-z0-9._~+/=-]{20,}")
                    .unwrap(),
                label: "AUTH_HEADER",
            },
        ]
    })
}

/// Return a copy of `text` with any matched secret patterns replaced by
/// `[REDACTED <LABEL>]`. When nothing matches the input is returned unchanged.
///
/// Also renders any session-scoped secret markers (`[[secret:…]]`) as
/// their `preview` form first, so an accidentally-leaked marker reads
/// as `sk-an...wXyZ` rather than the full bracket grammar. Order is
/// independent for correctness — markers don't share characters with
/// any credential regex — but doing markers first keeps the regex
/// scan working on shorter strings.
pub fn redact_secrets(text: &str) -> String {
    let mut out = workforce_common::secret_marker::render_markers_as_preview(text);
    for p in patterns() {
        if p.regex.is_match(&out) {
            out = p
                .regex
                .replace_all(&out, format!("[REDACTED {}]", p.label))
                .into_owned();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_github_classic_pat() {
        let s = concat!("token: ghp_EX", "AMPLE00000000000000000000000000000 end");
        assert_eq!(redact_secrets(s), "token: [REDACTED GITHUB_PAT] end");
    }

    #[test]
    fn redacts_fine_grained_pat() {
        let long = "a".repeat(82);
        let s = format!("key: github_pat_{long}!");
        let r = redact_secrets(&s);
        assert!(r.contains("[REDACTED GITHUB_PAT_FINE_GRAINED]"), "got: {r}");
    }

    #[test]
    fn redacts_anthropic_key() {
        let s = concat!(
            "sk-ant",
            "-api03-abcdefghijklmnopqrstuvwxyz0123456789_-abcdefghij end"
        );
        assert!(redact_secrets(s).contains("[REDACTED ANTHROPIC_KEY]"));
    }

    #[test]
    fn redacts_openai_classic() {
        let key = "sk-".to_string() + &"a".repeat(48);
        let s = format!("using {key} now");
        assert_eq!(redact_secrets(&s), "using [REDACTED OPENAI_KEY] now");
    }

    #[test]
    fn redacts_aws_access_key() {
        assert_eq!(
            redact_secrets("key AKIAIOSFODNN7EXAMPLE end"),
            "key [REDACTED AWS_ACCESS_KEY] end"
        );
    }

    #[test]
    fn redacts_slack_bot_token() {
        let s = concat!("token xoxb-", "123456789-987654321-abcdefghijklmn end");
        assert!(redact_secrets(s).contains("[REDACTED SLACK_TOKEN]"));
    }

    #[test]
    fn redacts_bearer_header() {
        let s = "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9'";
        let r = redact_secrets(s);
        // Either the specific BEARER_TOKEN or the generic AUTH_HEADER pattern
        // can match this — both are correct. What matters is the literal token
        // is gone.
        assert!(r.contains("REDACTED"));
        assert!(!r.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let s = "The PR fixes `address2: undefined` in `AddressAutocomplete.tsx`.";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn redacts_multiple_secrets_in_one_message() {
        let s = concat!(
            "ghp_ab",
            "cdefghijklmnopqrstuvwxyz0123456789 and AKIAIOSFODNN7EXAMPLE"
        );
        let r = redact_secrets(s);
        assert!(r.contains("[REDACTED GITHUB_PAT]"));
        assert!(r.contains("[REDACTED AWS_ACCESS_KEY]"));
        assert!(!r.contains("ghp_"));
        assert!(!r.contains("AKIA"));
    }

    #[test]
    fn specific_pattern_wins_over_generic_bearer() {
        let s = concat!(
            "Authorization: Bearer ghp_ab",
            "cdefghijklmnopqrstuvwxyz0123456789"
        );
        let r = redact_secrets(s);
        // The ghp_ match runs first and rewrites the tail; what's left is
        // "Authorization: Bearer [REDACTED GITHUB_PAT]" — the generic sweep
        // should not then double-redact that into [REDACTED AUTH_HEADER]
        // (brackets and uppercase don't match [A-Za-z0-9._~+/=-]{20,}).
        assert!(r.contains("[REDACTED GITHUB_PAT]"));
        assert_eq!(r.matches("REDACTED").count(), 1);
    }
}
