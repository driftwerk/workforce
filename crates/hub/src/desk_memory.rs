//! Hides secrets in what Desk's session memory keeps, with every pattern
//! the hub knows: its own outbound list and the shared catalogue's. Unlike
//! the log and outbound redactors, nothing of a secret is kept: no prefix,
//! no suffix, no preview. Each match is replaced whole by its label, and a
//! secret marker (`[[secret:…]]`) is removed whole, its preview with it.

use std::sync::LazyLock;

use regex::Regex;
use workforce_common::secret_marker::{parse_markers_spanned, NAMED_LABEL};
use workforce_desk::memory::{Redacted, Redactor};

/// A marker the grammar doesn't parse (a torn or hand-made one) is still a
/// marker: removed whole.
static ANY_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[secret:[^\]]*\]\]").expect("marker regex must compile"));

/// The storage redactor the hub installs in the desk.
pub struct HubRedactor;

/// What a secret is replaced by.
fn label(name: &str) -> String {
    format!("[REDACTED {name}]")
}

impl HubRedactor {
    /// `text` with each marker replaced by its label alone.
    fn markers(text: &str, count: &mut u32) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (range, marker) in parse_markers_spanned(text) {
            if range.start < at {
                continue;
            }
            out.push_str(&text[at..range.start]);
            let name = match marker.label.as_str() {
                NAMED_LABEL => "SECRET",
                other => other,
            };
            out.push_str(&label(name));
            *count += 1;
            at = range.end;
        }
        out.push_str(&text[at..]);
        ANY_MARKER
            .replace_all(&out, |_: &regex::Captures<'_>| {
                *count += 1;
                label("SECRET")
            })
            .into_owned()
    }

    /// `text` with every match of `regex` replaced whole by `name`, by span
    /// (never by value), leaving what a label already stands in for alone.
    fn pattern(text: &str, name: &str, regex: &Regex, count: &mut u32) -> String {
        if !regex.is_match(text) {
            return text.to_string();
        }
        regex
            .replace_all(text, |c: &regex::Captures<'_>| {
                let m = c.get(0).expect("capture 0 is the match");
                if workforce_common::redact::is_public(m.as_str()) {
                    return m.as_str().to_string();
                }
                *count += 1;
                label(name)
            })
            .into_owned()
    }
}

impl Redactor for HubRedactor {
    fn stored(&self, text: &str) -> Redacted {
        let mut count = 0;
        let mut out = Self::markers(text, &mut count);
        // The hub's own list first: its labels are the ones outbound
        // surfaces already show. Then the shared catalogue, whose generic
        // sweeps (`key = …`, PEM blocks) catch what neither names.
        for (name, regex) in
            crate::redact::labelled_patterns().chain(workforce_common::redact::labelled_patterns())
        {
            out = Self::pattern(&out, name, regex, &mut count);
        }
        Redacted { text: out, count }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::secret_marker::{Marker, MarkerId};

    /// A secret of each pattern family, built in parts so no whole one is
    /// written in the source.
    fn samples() -> Vec<(&'static str, String)> {
        vec![
            (
                "ANTHROPIC_KEY",
                ["sk-ant", "-api03-", &"Ab3".repeat(15)].concat(),
            ),
            ("OPENAI_KEY", ["sk-", &"aZ9".repeat(16)].concat()),
            (
                "OPENAI_PROJECT_KEY",
                ["sk-proj-", &"Qx7".repeat(15)].concat(),
            ),
            ("GITHUB_PAT", ["ghp_", &"Mn4".repeat(12)].concat()),
            ("GITLAB_PAT", ["glpat-", &"Hk2".repeat(8)].concat()),
            (
                "SLACK_TOKEN",
                ["xoxb-", "1234567890-", &"Rt5".repeat(8)].concat(),
            ),
            ("AWS_ACCESS_KEY", ["AKIA", "IOSFODNN7EXAMPLE"].concat()),
            ("GOOGLE_API_KEY", ["AIza", &"Sy8".repeat(11), "Ab"].concat()),
            ("STRIPE_SECRET", ["sk_", "live_", &"Wq6".repeat(9)].concat()),
            (
                "JWT",
                [
                    "eyJ",
                    "hbGciOiJIUzI1NiJ9",
                    ".eyJ",
                    "zdWIiOiIxIn0",
                    ".",
                    "Ug5PQr7Tz",
                ]
                .concat(),
            ),
            (
                "PEM_PRIVATE_KEY",
                [
                    "-----BEGIN RSA PRIVATE ",
                    "KEY-----\nMIIEowIBAAKCAQEA",
                    "\n-----END RSA PRIVATE KEY-----",
                ]
                .concat(),
            ),
            ("ENV_CREDENTIAL", ["api_key=", &"Zp3".repeat(8)].concat()),
            ("BEARER_TOKEN", ["Bearer ", &"Vb1".repeat(9)].concat()),
        ]
    }

    #[test]
    fn each_pattern_family_is_replaced_whole_and_nothing_of_it_kept() {
        for (family, secret) in samples() {
            let text = format!("before {secret} after");
            let out = HubRedactor.stored(&text);
            let n = secret.chars().count();
            let head: String = secret.chars().take(4).collect();
            let tail: String = secret.chars().skip(n - 4).collect();
            assert!(
                out.text.starts_with("before [REDACTED ") && out.text.ends_with("] after"),
                "{family}: the secret is replaced by a label"
            );
            assert!(
                !out.text.contains(&head) && !out.text.contains(&tail),
                "{family}: no prefix or suffix is kept"
            );
            assert!(out.count >= 1, "{family}: counted");
        }
    }

    #[test]
    fn markers_are_removed_whole_with_their_preview() {
        let auto = Marker {
            id: MarkerId::fresh(),
            label: "ANTHROPIC_KEY".into(),
            preview: "sk-a...wXyZ".into(),
        }
        .render();
        let named = Marker {
            id: MarkerId::named("deploy_token").unwrap(),
            label: NAMED_LABEL.into(),
            preview: "***".into(),
        }
        .render();
        let torn = "[[secret:not a marker]]";
        let out = HubRedactor.stored(&format!("a {auto} b {named} c {torn} d"));
        assert_eq!(
            out.text,
            "a [REDACTED ANTHROPIC_KEY] b [REDACTED SECRET] c [REDACTED SECRET] d"
        );
        assert_eq!(out.count, 3);
        assert!(!out.text.contains("wXyZ") && !out.text.contains("deploy_token"));
    }

    #[test]
    fn ordinary_text_and_public_keys_are_left_alone() {
        let text = "The PR fixes `address2: undefined` in `AddressAutocomplete.tsx`.";
        assert_eq!(
            HubRedactor.stored(text),
            Redacted {
                text: text.into(),
                count: 0
            }
        );
        let public = ["pk_", "live_", &"Wq6".repeat(9)].concat();
        assert_eq!(HubRedactor.stored(&public).count, 0);
    }
}
