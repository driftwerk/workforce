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

/// The patterns with their labels, the most specific first.
pub(crate) fn labelled_patterns() -> impl Iterator<Item = (&'static str, &'static Regex)> {
    patterns().iter().map(|p| (p.label, &p.regex))
}

/// Return a copy of `text` with any matched secret patterns replaced whole
/// by `[REDACTED <LABEL>]`, keeping no prefix or suffix of a secret. When
/// nothing matches the input is returned unchanged.
///
/// Any session-scoped secret marker (`[[secret:…]]`) is rendered first as
/// its preview, which never holds a character of the secret. Every
/// pattern is then matched over that text and overlapping matches merged
/// ([`workforce_common::redact::redact_spans`]), so a shorter pattern never
/// leaves a longer secret's tail behind.
pub fn redact_secrets(text: &str) -> String {
    let out = workforce_common::secret_marker::render_markers_as_preview(text);
    let found = labelled_patterns()
        .flat_map(|(label, regex)| {
            regex
                .find_iter(&out)
                .map(move |m| (m.start(), m.end(), label))
        })
        .collect();
    workforce_common::redact::redact_spans(&out, found)
}

/// `text` with every secret the hub's list or the shared catalogue knows
/// replaced whole by `[REDACTED <LABEL>]`: nothing of it kept, no prefix,
/// no suffix, no preview, and a secret marker (`[[secret:…]]`) removed with
/// its preview. All patterns are matched over the original text and
/// overlapping matches merged, so a shorter pattern never leaves a longer
/// secret's tail behind. For text a person reads back later: a step's
/// advice, a failed CLI's last lines.
pub fn hide_secrets(text: &str) -> String {
    static MARKER: OnceLock<Regex> = OnceLock::new();
    let marker = MARKER.get_or_init(|| Regex::new(r"\[\[secret:[^\]]*\]\]").expect("marker regex"));
    let mut found: Vec<(usize, usize, &'static str)> = marker
        .find_iter(text)
        .map(|m| (m.start(), m.end(), "SECRET"))
        .collect();
    for (label, regex) in labelled_patterns().chain(workforce_common::redact::labelled_patterns()) {
        for m in regex.find_iter(text) {
            if !workforce_common::redact::is_public(m.as_str()) {
                found.push((m.start(), m.end(), label));
            }
        }
    }
    workforce_common::redact::redact_spans(text, found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_secrets_keep_nothing_of_themselves() {
        let jwt = concat!(
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9",
            ".dGhpcyBpcyBub3QganNvbiBidXQgYSBwYXlsb2Fk",
            ".c2lnbmF0dXJlLWJ5dGVzLWhlcmUtMTIzNDU2"
        );
        let code = "4/0AVG7fiQ1example_Authorization-Code_value";
        let key = format!("sk-proj-{}", "Ab12Cd34".repeat(4));
        let marker = "[[secret:ab12:sk-pr...wXyZ]]";
        let text = format!(
            "jwt {jwt}\nredirect https://app.test/cb?code={code}&state=xyz\nkey {key} {marker} exit code=1"
        );
        let hidden = hide_secrets(&text);
        for secret in [
            jwt,
            code,
            key.as_str(),
            "sk-pr...wXyZ",
            "Ab12Cd34",
            "4/0AVG",
        ] {
            assert!(!hidden.contains(secret), "{secret} in {hidden}");
        }
        assert!(hidden.contains("jwt [REDACTED JWT]"), "{hidden}");
        assert!(
            hidden.contains("cb?[REDACTED OAUTH_CODE]&state=xyz"),
            "{hidden}"
        );
        assert!(hidden.contains("[REDACTED OPENAI_PROJECT_KEY]"), "{hidden}");
        assert!(hidden.contains("[REDACTED SECRET]"), "{hidden}");
        assert!(
            hidden.ends_with("exit code=1"),
            "a short code is no secret: {hidden}"
        );
        assert_eq!(hide_secrets("nothing here"), "nothing here");
    }

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
        // The generic header and bearer matches hold the ghp_ one: merged,
        // they are one secret, named by the most specific pattern.
        assert_eq!(r, "[REDACTED GITHUB_PAT]");
    }

    /// A shorter match inside a longer secret never leaves its head or
    /// tail behind: the spans are merged and hidden whole.
    #[test]
    fn no_head_or_tail_of_a_secret_is_left() {
        let pat = concat!("ghp_ab", "cdefghijklmnopqrstuvwxyz0123456789");
        let s = format!("Authorization: token {pat}.and-a-tail-0123456789abcdef");
        let r = redact_secrets(&s);
        assert!(!r.contains("ghp_") && !r.contains("6789"), "{r}");
        let bearer = format!("bearer {pat}~tail-0123456789abcdefghij");
        let r = redact_secrets(&bearer);
        assert!(!r.contains("ghp_") && !r.contains("6789"), "{r}");
    }
}
