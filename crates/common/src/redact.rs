//! Secret redaction for log output, plus session-scoped tokenization
//! for text destined for the LLM.
//!
//! Two modes of operation share the same pattern set:
//!
//!  * [`redact_secrets`] / [`contains_secret`] — log-side defence. A
//!    detected secret is replaced with `first6...last4` (e.g.
//!    `sk-ant...ABCD`); call this on anything heading to disk, stderr,
//!    or a tracing event.
//!  * [`tokenize_secrets`] — LLM-side defence. A detected secret is
//!    replaced with an opaque marker like
//!    `[[secret:<id> type=ANTHROPIC_KEY preview=sk-an...wXyZ]]` and the
//!    cleartext is stashed in the per-session
//!    [`crate::secret_marker::SecretCache`]. The LLM only ever sees the
//!    marker; allowlisted "secret-sink" tools (vault.set,
//!    system.add_account, slack.connect) substitute it back to
//!    cleartext at the tool boundary.
//!
//! Both modes use the same labelled pattern set, ordered most-specific
//! first so a `ghp_…` match is labelled `GITHUB_PAT` rather than caught
//! by the generic env-var sweep.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

use crate::secret::Secret;
#[cfg(test)]
use crate::secret_marker::MarkerId;
use crate::secret_marker::{global_cache, preview_of, CapturedSecret, Marker, SecretSource};

struct LabelledPattern {
    /// Audit / marker label. Uppercase + digits + underscore.
    label: &'static str,
    regex: Regex,
}

/// The words that name a credential, shared by the text sweeps and
/// [`is_credential_field`].
const CREDENTIAL_KEYWORDS: &str = "api[_-]?key|api[_-]?token|access[_-]?token|auth[_-]?token|secret[_-]?key|client[_-]?secret|webhook[_-]?secret|apikey|authorization|password|passwd|secret|token";

static ENV_CREDENTIAL: Lazy<String> = Lazy::new(|| {
    format!(
        r#"(?i)\b(?:{CREDENTIAL_KEYWORDS}|key)\b\s*[=:]\s*['"]?([a-zA-Z0-9._+/=_-]{{20,}})['"]?"#
    )
});

/// Field names that hold a credential without one of [`CREDENTIAL_KEYWORDS`]
/// in them. Kept out of the keyword list so the text sweeps match what they
/// always have.
const CREDENTIAL_FIELD_NAMES: &str = "credentials?|key[_-]?pem|(?:private|signing|encryption|master|ssh|access)[_-]?key|passphrase|dsn|connection[_-]?string|webhook[_-]?url|bearer|cookies?|headers|env";

static CREDENTIAL_FIELD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"(?i)(?:^key$|(?:^|[_.-])(?:{CREDENTIAL_KEYWORDS}|{CREDENTIAL_FIELD_NAMES})$)"
    ))
    .expect("invalid credential-field regex")
});

/// Whether a config or payload field named `name` holds a credential
/// (`api_key`, `auth_token`, `client_secret`, `dsn`) or a table of them
/// (`headers`, `env`): its value is never shown, only whether it is set.
/// Counts (`max_tokens`) and settings about a credential (`requires_key`,
/// `token_ttl_days`) are not.
pub fn is_credential_field(name: &str) -> bool {
    CREDENTIAL_FIELD.is_match(name)
}

/// Labelled patterns for known secret formats. Order matters — the
/// most specific pattern for a given prefix MUST come first
/// (`sk-ant-…` before generic `sk-…`). The catalogue aims to cover every
/// credential format an agent or skill is likely to encounter; vendors
/// without a fixed prefix are caught indirectly via `ENV_CREDENTIAL`.
static LABELLED_PATTERNS: Lazy<Vec<LabelledPattern>> = Lazy::new(|| {
    let raw: &[(&str, &str)] = &[
        // ── LLM providers ──
        // OAuth tokens (`sk-ant-oat…`, from `claude setup-token`) MUST precede
        // the generic `sk-ant-` API-key pattern — the latter would otherwise
        // swallow them under the wrong label, leading agents to tell users they
        // pasted an API key when they pasted a Max subscription OAuth token.
        ("ANTHROPIC_OAUTH_TOKEN", r"sk-ant-oat[a-zA-Z0-9_-]{17,}"),
        ("ANTHROPIC_KEY", r"sk-ant-[a-zA-Z0-9_-]{20,}"),
        ("OPENAI_PROJECT_KEY", r"sk-proj-[a-zA-Z0-9_-]{20,}"),
        ("OPENROUTER_KEY", r"sk-or-v1-[a-zA-Z0-9]{32,}"),
        ("OPENROUTER_LEGACY_KEY", r"sk-or-[a-zA-Z0-9]{20,}"),
        ("OPENAI_KEY", r"sk-[a-zA-Z0-9]{20,}"),
        ("PERPLEXITY_KEY", r"pplx-[a-zA-Z0-9]{32,}"),
        ("REPLICATE_KEY", r"r8_[a-zA-Z0-9]{32,}"),
        ("FIREWORKS_KEY", r"fw_[a-zA-Z0-9]{32,}"),
        ("HUGGINGFACE_KEY", r"hf_[a-zA-Z0-9]{32,}"),
        ("VOYAGE_KEY", r"pa-[a-zA-Z0-9_-]{32,}"),
        ("GROQ_KEY", r"gsk_[a-zA-Z0-9]{20,}"),
        ("GOOGLE_API_KEY", r"AIza[a-zA-Z0-9_-]{35}"),
        // ── Source-control providers ──
        ("GITHUB_PAT", r"ghp_[a-zA-Z0-9]{36,}"),
        ("GITHUB_OAUTH_TOKEN", r"gho_[a-zA-Z0-9]{36,}"),
        ("GITHUB_SERVER_TOKEN", r"ghs_[a-zA-Z0-9]{36,}"),
        ("GITHUB_USER_TOKEN", r"ghu_[a-zA-Z0-9]{36,}"),
        ("GITHUB_PAT_FINE_GRAINED", r"github_pat_[a-zA-Z0-9_]{20,}"),
        ("GITLAB_PAT", r"glpat-[a-zA-Z0-9_-]{20,}"),
        // ── SaaS integrations ──
        ("SLACK_TOKEN", r"xox[bpras]-[a-zA-Z0-9-]{20,}"),
        ("SLACK_APP_TOKEN", r"xapp-[a-zA-Z0-9-]{20,}"),
        ("LINEAR_API_TOKEN", r"lin_api_[a-zA-Z0-9]{20,}"),
        ("ATLASSIAN_API_TOKEN", r"ATATT[a-zA-Z0-9_-]{20,}"),
        ("NOTION_TOKEN", r"ntn_[a-zA-Z0-9]{30,}"),
        ("NOTION_LEGACY_TOKEN", r"secret_[a-zA-Z0-9]{43}"),
        ("HUBSPOT_PAT", r"pat-(?:na1|eu1)-[a-f0-9-]{36}"),
        ("DATADOG_APP_KEY", r"ddapp_[a-zA-Z0-9]{20,}"),
        ("POSTHOG_PERSONAL_API_KEY", r"phx_[A-Za-z0-9]{32,}"),
        ("SENDGRID_KEY", r"SG\.[a-zA-Z0-9_-]{22}\.[a-zA-Z0-9_-]{43}"),
        ("SENTRY_USER_TOKEN", r"sntryu_[a-zA-Z0-9_]{60,}"),
        ("SENTRY_ORG_TOKEN", r"sntrys_[a-zA-Z0-9_]{60,}"),
        ("TWILIO_ACCOUNT_SID", r"\bAC[a-f0-9]{32}\b"),
        (
            "DISCORD_BOT_TOKEN",
            r"\b[MN][A-Za-z0-9_-]{23}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27}\b",
        ),
        ("TELEGRAM_BOT_TOKEN", r"\b\d{8,10}:[A-Za-z0-9_-]{35}\b"),
        ("ASANA_PAT", r"1/\d{16}:[a-f0-9]{32}"),
        ("DIGITALOCEAN_TOKEN", r"do[oprt]_v1_[a-f0-9]{64}"),
        ("BRAVE_SEARCH_KEY", r"BSA[a-zA-Z0-9_-]{20,}"),
        ("WORKFORCE_PAIRING_TOKEN", r"wfp_[a-zA-Z0-9_-]{20,}"),
        ("WORKFORCE_REMOTE_MCP_TOKEN", r"wfr_[a-zA-Z0-9_-]{20,}"),
        // ── Payments ──
        ("STRIPE_SECRET", r"(?:sk|rk)_(?:live|test)_[a-zA-Z0-9]{24,}"),
        ("STRIPE_WEBHOOK_SECRET", r"whsec_[a-zA-Z0-9]{32,}"),
        // ── Cloud providers ──
        ("AWS_ACCESS_KEY", r"AKIA[0-9A-Z]{16}"),
        (
            "AWS_SECRET_ACCESS_KEY",
            r#"(?i)aws[_-]?secret[_-]?access[_-]?key\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#,
        ),
        ("AZURE_STORAGE_KEY", r"AccountKey=[A-Za-z0-9+/=]{88}"),
        ("AGE_SECRET_KEY", r"AGE-SECRET-KEY-[A-Z0-9]{20,}"),
        (
            "HETZNER_API_TOKEN",
            r#"(?:hcloud|HCLOUD|hetzner|HETZNER)[_\s]*(?:token|TOKEN|api_key|API_KEY)\s*[=:]\s*['"]?([a-fA-F0-9]{64})['"]?"#,
        ),
        // ── Dev / CI registries ──
        ("NPM_TOKEN", r"npm_[a-zA-Z0-9]{20,}"),
        ("DOCKER_HUB_PAT", r"dckr_pat_[a-zA-Z0-9_-]{27,}"),
        ("PYPI_TOKEN", r"pypi-AgEIcHlwaS5vcmc[a-zA-Z0-9_-]{50,}"),
        ("RUBYGEMS_KEY", r"rubygems_[a-f0-9]{48}"),
        // ── Generic high-value formats ──
        (
            "JWT",
            r"eyJ[A-Za-z0-9_=-]+\.eyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+",
        ),
        ("BASIC_AUTH", r"Basic\s+[A-Za-z0-9+/=]{20,}"),
        ("BEARER_TOKEN", r"Bearer\s+[a-zA-Z0-9._-]{20,}"),
        // ── Context-keyword fallback (catches unknown vendors) ──
        // Widened from the original uppercase-only list to: case-insensitive,
        // hyphenated variants (`api-key`), and common keyword aliases.
        ("ENV_CREDENTIAL", ENV_CREDENTIAL.as_str()),
        (
            "PEM_PRIVATE_KEY",
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
        ),
    ];

    raw.iter()
        .map(|(label, pattern)| LabelledPattern {
            label,
            regex: Regex::new(pattern).expect("invalid redaction regex"),
        })
        .collect()
});

/// Allowlist of patterns that look like credentials but are public by
/// design (e.g. Stripe publishable keys). A match whose text contains an
/// allowlisted span is left untouched even when another pattern would
/// otherwise capture it.
static ALLOWLIST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    let raw: &[&str] = &[
        // Stripe publishable keys are intentionally public.
        r"pk_(?:live|test)_[a-zA-Z0-9]{24,}",
    ];
    raw.iter()
        .map(|p| Regex::new(p).expect("invalid allowlist regex"))
        .collect()
});

fn is_allowlisted(matched_text: &str) -> bool {
    ALLOWLIST_PATTERNS.iter().any(|p| p.is_match(matched_text))
}

/// Regex matching the wire form of a substituted secret marker. Used to
/// guard subsequent pattern passes from re-matching content inside a
/// marker we already inserted (e.g. the widened `ENV_CREDENTIAL` would
/// otherwise re-capture `secret:<id>` from the marker itself).
static MARKER_SPAN_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[\[secret:[^\]]+\]\]").expect("invalid marker regex"));

fn marker_spans(text: &str) -> Vec<(usize, usize)> {
    MARKER_SPAN_PATTERN
        .find_iter(text)
        .map(|m| (m.start(), m.end()))
        .collect()
}

fn overlaps_any(start: usize, end: usize, spans: &[(usize, usize)]) -> bool {
    spans.iter().any(|(s, e)| start < *e && *s < end)
}

/// Context-keyword sweep for the warn-on-miss telemetry hook. Catches
/// patterns like `api_key: <high-entropy-value>` that none of the
/// catalogued patterns matched — surfaces vendors we should add.
static SUSPICIOUS_CONTEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?i)\b(?:{CREDENTIAL_KEYWORDS}|bearer)\b\s*[:=]\s*['"]?([A-Za-z0-9._+/=_-]{{20,}})"#
    ))
    .expect("invalid suspicious-context regex")
});

/// Backwards-compatible accessor for the legacy unlabeled API.
static SECRET_PATTERNS: Lazy<Vec<&'static Regex>> =
    Lazy::new(|| LABELLED_PATTERNS.iter().map(|p| &p.regex).collect());

/// Mask a secret value, showing only the first 6 and last 4 characters.
fn mask_secret(secret: &str) -> String {
    if secret.len() <= 10 {
        return "***REDACTED***".to_string();
    }
    // Use char-boundary-safe slicing to avoid panicking on multi-byte characters
    let first_end = secret.floor_char_boundary(6);
    let last_start = secret.ceil_char_boundary(secret.len().saturating_sub(4));
    let first = &secret[..first_end];
    let last = &secret[last_start..];
    format!("{first}...{last}")
}

/// Redact all detected secrets in a string.
pub fn redact_secrets(input: &str) -> String {
    let mut result = input.to_string();

    for pattern in SECRET_PATTERNS.iter() {
        let spans = marker_spans(&result);
        result = pattern
            .replace_all(&result, |caps: &regex::Captures| {
                let m = caps.get(0).expect("regex capture 0 missing");
                let full = m.as_str();
                if overlaps_any(m.start(), m.end(), &spans) || is_allowlisted(full) {
                    full.to_string()
                } else {
                    mask_secret(full)
                }
            })
            .to_string();
    }

    result
}

/// Check if a string likely contains secrets (excluding allowlisted
/// public values like Stripe publishable keys).
pub fn contains_secret(input: &str) -> bool {
    SECRET_PATTERNS
        .iter()
        .any(|p| p.find_iter(input).any(|m| !is_allowlisted(m.as_str())))
}

/// Redact secrets in every string leaf of a JSON value, in place.
/// Recurses objects and arrays up to `max_depth` (a guard against
/// pathological nesting); deeper leaves are left untouched. Returns
/// whether any leaf changed. Object keys are not rewritten — only values.
pub fn redact_json_strings(value: &mut serde_json::Value, max_depth: u8) -> bool {
    match value {
        serde_json::Value::String(s) => {
            let redacted = redact_secrets(s);
            if redacted != *s {
                *s = redacted;
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(items) if max_depth > 0 => items
            .iter_mut()
            .fold(false, |acc, v| redact_json_strings(v, max_depth - 1) || acc),
        serde_json::Value::Object(map) if max_depth > 0 => map
            .values_mut()
            .fold(false, |acc, v| redact_json_strings(v, max_depth - 1) || acc),
        _ => false,
    }
}

// ───────────────────────────────────────────────────────────────────────
// LLM-side tokenization
// ───────────────────────────────────────────────────────────────────────

/// Replace every detected secret in `text` with an opaque marker and
/// stash the cleartext in the per-session [`SecretCache`].
///
/// Returns the marker-substituted text alongside metadata about each
/// captured secret — the caller (slack_manager, gateway, …) uses these
/// to surface a "captured securely; please delete the original message"
/// nudge to the user.
///
/// If no global cache is installed (CLI tools, certain test paths), the
/// function falls back to redacting in-place via the same form as
/// [`redact_secrets`] and emits a `sentinel.audit` warning so the gap
/// is visible. This keeps the function safe-by-default — the worst case
/// is the LLM sees `[REDACTED first6...last4]` instead of a usable
/// marker, never cleartext.
///
/// The pattern list is applied most-specific first; once a region is
/// replaced with a marker, no other pattern can match the marker text
/// (the marker grammar uses `[[`/`]]` delimiters that don't appear in
/// any credential pattern).
pub fn tokenize_secrets(
    text: &str,
    session_key: &str,
    source: SecretSource,
) -> (String, Vec<CapturedSecret>) {
    if text.is_empty() {
        return (String::new(), Vec::new());
    }

    let Some(cache) = global_cache() else {
        // No cache installed — never let cleartext through. The audit
        // line surfaces the misconfiguration.
        if contains_secret(text) {
            tracing::warn!(
                target: "sentinel.audit",
                session_key = %session_key,
                "tokenize_secrets called with no global cache installed — falling back to in-place redaction"
            );
            return (redact_secrets(text), Vec::new());
        }
        return (text.to_string(), Vec::new());
    };

    let mut result = text.to_string();
    let mut captured = Vec::new();

    // Matches are replaced by span, never by value: a value that is also a
    // substring of a longer match must not be rewritten inside it.
    for pattern in LABELLED_PATTERNS.iter() {
        let marker_spans = marker_spans(&result);
        let mut rendered: HashMap<String, String> = HashMap::new();
        result = pattern
            .regex
            .replace_all(&result, |caps: &regex::Captures| {
                let m = caps.get(0).expect("regex capture 0 missing");
                let matched = m.as_str();
                if overlaps_any(m.start(), m.end(), &marker_spans) || is_allowlisted(matched) {
                    return matched.to_string();
                }
                if let Some(marker) = rendered.get(matched) {
                    return marker.clone();
                }
                let preview = preview_of(matched);
                let id = cache.capture(
                    session_key,
                    Secret::new(matched.to_string()),
                    pattern.label,
                    source.clone(),
                );
                let marker = Marker {
                    id: id.clone(),
                    label: pattern.label.to_string(),
                    preview: preview.clone(),
                }
                .render();
                captured.push(CapturedSecret {
                    id,
                    label: pattern.label.to_string(),
                    preview,
                    source: source.clone(),
                });
                rendered.insert(matched.to_string(), marker.clone());
                marker
            })
            .into_owned();
    }

    // By-value safety net: scrub any *already-captured* cleartext that
    // re-entered as raw text (e.g. a persisted `/secret …` message read
    // back from Slack thread history, or a value surfaced by a
    // Slack-history tool result). The pattern loop above can't catch an
    // arbitrary password — it matches no known token shape — but an
    // exact-match against the cache can.
    let result = cache.redact_known(session_key, &result);

    warn_on_unredacted_credentials(&result, session_key);

    (result, captured)
}

/// Post-pass that fires after every redaction pass. If the surviving
/// text still contains a `<keyword>: <high-entropy-value>` shape and the
/// value isn't already a marker / mask, log a `sentinel.audit` warning.
/// This is the feedback loop for catalogue gaps — a missed vendor shows
/// up in logs so the pattern can be added in a follow-up.
fn warn_on_unredacted_credentials(text: &str, session_key: &str) {
    for caps in SUSPICIOUS_CONTEXT.captures_iter(text) {
        let Some(val) = caps.get(1) else { continue };
        let val_str = val.as_str();
        if val_str.starts_with("[[secret:")
            || val_str.contains("...")
            || val_str.starts_with("***")
            || is_allowlisted(val_str)
        {
            continue;
        }
        tracing::warn!(
            target: "sentinel.audit",
            session_key = %session_key,
            preview = %preview_of(val_str),
            length = val_str.len(),
            "context keyword near high-entropy value but no pattern matched — likely missing vendor pattern in redact.rs"
        );
    }
}

/// Redact a known-secret value to `abcd…1234` form (first 4 / last 4
/// chars). Values shorter than 16 chars collapse to `***` so tiny
/// secrets don't leak via short prefixes.
///
/// Used by LLM-facing tool boundaries that need to confirm "a credential
/// is configured" without disclosing the value itself. Distinct from
/// [`redact_secrets`], which scans free-form strings for known token
/// patterns; this one operates on a value already known to be a secret.
pub fn redact_value(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 16 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

const UNPARSEABLE_URL: &str = "<unparseable url>";

/// `url` reduced to scheme, host and any non-default port. Userinfo, query
/// and fragment can hold credentials, and so can the path of a capability
/// URL, so none of them survive.
pub fn redact_url(url: &Url) -> Url {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_path("");
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted
}

/// [`redact_url`] for a URL still in text form. Input that does not parse
/// is never echoed, since nothing in it can be told apart from a secret.
pub fn redact_url_str(raw: &str) -> String {
    Url::parse(raw).map_or_else(
        |_| UNPARSEABLE_URL.to_string(),
        |url| redact_url(&url).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL_SECRETS: [&str; 5] = ["opsuser7", "hunter2", "k-northwind42", "s3cr3t", "fr4gment"];

    const URL_CASES: &[(&str, &str)] = &[
        (
            "https://opsuser7:hunter2@mcp.acme.example/mcp",
            "https://mcp.acme.example/",
        ),
        (
            "https://opsuser7@mcp.acme.example/mcp",
            "https://mcp.acme.example/",
        ),
        (
            "https://:hunter2@mcp.acme.example/",
            "https://mcp.acme.example/",
        ),
        (
            "https://opsuser7:hun%40ter2@mcp.acme.example/",
            "https://mcp.acme.example/",
        ),
        (
            "https://mcp.acme.example/mcp?sig=s3cr3t&api_key=x",
            "https://mcp.acme.example/",
        ),
        (
            "https://mcp.acme.example/mcp#fr4gment",
            "https://mcp.acme.example/",
        ),
        (
            "https://gw.northwind.example/mcp/k-northwind42/sse",
            "https://gw.northwind.example/",
        ),
        (
            "http://opsuser7:hunter2@[::1]:8443/mcp?sig=s3cr3t",
            "http://[::1]:8443/",
        ),
        (
            "https://mcp.acme.example:8443/mcp",
            "https://mcp.acme.example:8443/",
        ),
        (
            "https://mcp.acme.example:443/mcp",
            "https://mcp.acme.example/",
        ),
        (
            "custom://opsuser7:hunter2@h.example:9/x?sig=s3cr3t",
            "custom://h.example:9",
        ),
        ("mailto:hunter2@acme.example", "mailto:"),
        ("https://opsuser7:hunter2@[bad", "<unparseable url>"),
        ("hunter2", "<unparseable url>"),
    ];

    #[test]
    fn a_redacted_url_keeps_only_scheme_host_and_port() {
        for (input, expected) in URL_CASES {
            assert_eq!(redact_url_str(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn redacting_a_url_twice_changes_nothing() {
        for url in URL_CASES
            .iter()
            .filter_map(|(input, _)| Url::parse(input).ok())
        {
            assert_eq!(redact_url(&redact_url(&url)), redact_url(&url), "{url}");
        }
    }

    #[test]
    fn no_url_secret_survives_redaction() {
        for (input, _) in URL_CASES {
            let redacted = redact_url_str(input);
            for secret in URL_SECRETS {
                assert!(!redacted.contains(secret), "{input} -> {redacted}");
            }
        }
    }

    #[test]
    fn a_credential_field_is_named_by_its_last_word_not_by_a_count_or_a_flag() {
        for name in [
            "api_key",
            "embedding_api_key",
            "token",
            "auth_token",
            "jwt_secret",
            "client_secret",
            "password",
            "key_pem",
            "credentials",
            "API_KEY",
            "llm.api_key",
            "key",
            "signing_key",
            "private_key",
            "ssh-key",
            "passphrase",
            "dsn",
            "database_dsn",
            "connection_string",
            "webhook_url",
            "signing_secret",
            "authorization",
            "bearer",
            "cookie",
            "headers",
            "extra_headers",
            "env",
        ] {
            assert!(is_credential_field(name), "{name}");
        }
        for name in [
            "max_tokens",
            "tokens",
            "token_ttl_days",
            "token_endpoint",
            "requires_key",
            "key_file",
            "tier_key",
            "passive_min_turn_tokens",
            "model",
            "url",
            "environment",
            "keys_required",
            "key_source",
        ] {
            assert!(!is_credential_field(name), "{name}");
        }
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = concat!(
            "Using key sk-ant",
            "-oat01-abcdefghijklmnopqrstuvwxyz1234567890"
        );
        let result = redact_secrets(input);
        assert!(!result.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(result.contains("sk-ant...7890"));
    }

    #[test]
    fn redacts_github_pat() {
        let input = concat!("Token: ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");
        let result = redact_secrets(input);
        assert!(!result.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(result.contains("ghp_AB...1234"));
    }

    #[test]
    fn redacts_slack_token() {
        let input = concat!("Slack: xoxb-1", "234567890-abcdefghij-klmnopqrstuvwxyz");
        let result = redact_secrets(input);
        assert!(!result.contains("klmnopqrstuvwxyz"));
    }

    #[test]
    fn redacts_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature123456";
        let result = redact_secrets(input);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn redacts_env_var_pattern() {
        let input = concat!("API_KEY=sk_live_", "abcdef123456789012345678901234567890");
        let result = redact_secrets(input);
        assert!(!result.contains("abcdef123456789012345678901234567890"));
    }

    #[test]
    fn redacts_pem_key() {
        let input = "Found key: -----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...\n-----END RSA PRIVATE KEY-----";
        let result = redact_secrets(input);
        assert!(!result.contains("MIIEowIBAAK"));
    }

    #[test]
    fn preserves_non_secret_content() {
        let input = "Hello world, running cargo test in /tmp/workspace";
        let result = redact_secrets(input);
        assert_eq!(result, input);
    }

    #[test]
    fn redact_json_strings_redacts_nested_string_leaves() {
        let mut v = serde_json::json!({
            "headers": { "authorization": "Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature123456" },
            "items": [concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"), "plain text"],
            "count": 7
        });
        assert!(redact_json_strings(&mut v, 32));
        let rendered = v.to_string();
        assert!(!rendered.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!rendered.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(rendered.contains("plain text"));
        assert_eq!(v["count"], serde_json::json!(7));
    }

    #[test]
    fn redact_json_strings_is_noop_without_secrets_and_respects_depth() {
        let mut clean = serde_json::json!({ "a": ["b", { "c": "d" }], "n": 1 });
        assert!(!redact_json_strings(&mut clean, 32));
        // Below the depth bound the leaf is not reached, so no change.
        let mut deep = serde_json::json!({ "x": { "y": concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234") } });
        assert!(!redact_json_strings(&mut deep, 1));
    }

    #[test]
    fn detects_secrets() {
        assert!(contains_secret(concat!(
            "key is sk-ant",
            "-oat01-abcdefghijklmnop1234567890"
        )));
        assert!(!contains_secret("normal log message"));
    }

    #[test]
    fn multiple_secrets_in_one_string() {
        let input = concat!("Key1: ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234 Key2: sk-ant-oat01-zyxwvutsrqponmlkjihgfedcba09876");
        let result = redact_secrets(input);
        assert!(!result.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(!result.contains("zyxwvutsrqponmlkjihgfedcba"));
    }

    #[test]
    fn redacts_linear_api_token() {
        let input = concat!("Linear token: lin_ap", "i_aBcDeFgHiJkLmNoPqRsT1234567890");
        let result = redact_secrets(input);
        assert!(!result.contains("aBcDeFgHiJkLmNoPqRsT1234567890"));
        assert!(result.contains("lin_ap...7890"));
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_posthog_personal_api_key() {
        let input = concat!(
            "PostHog key: phx_",
            "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcd"
        );
        let result = redact_secrets(input);
        assert!(!result.contains("aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789abcd"));
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_brave_search_key() {
        let input = "Brave API key: BSAabcdefghij1234567890ABCDEFG";
        let result = redact_secrets(input);
        assert!(!result.contains("abcdefghij1234567890ABCDEFG"));
        assert!(result.contains("BSAabc...DEFG"));
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_workforce_pairing_token() {
        let input = "Pairing: wfp_abc123def456ghi789jkl012mno345pqr";
        let result = redact_secrets(input);
        assert!(!result.contains("abc123def456ghi789jkl012mno345pqr"));
        assert!(result.contains("wfp_ab...5pqr"));
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_age_secret_key() {
        let input = "Key: AGE-SECRET-KEY-1ABCDEFGHIJKLMNOPQRSTUVWXYZ234567890ABCDEFGHIJKLMNO";
        let result = redact_secrets(input);
        assert!(!result.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567890"));
        assert!(result.contains("AGE-SE...LMNO"));
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_hetzner_api_token() {
        let input = "hcloud_token=aabbccdd11223344556677889900aabbccdd11223344556677889900aabbccdd";
        let result = redact_secrets(input);
        assert!(
            !result.contains("aabbccdd11223344556677889900aabbccdd11223344556677889900aabbccdd")
        );
        assert!(contains_secret(input));
    }

    #[test]
    fn no_false_positive_on_common_text() {
        let inputs = [
            "The BSA meeting is scheduled for Tuesday",
            "Please review the linear algebra homework",
            "The age of the server is 42 days",
            "Build Status: All checks passed",
            "Running integration tests on wfp module",
        ];
        for input in &inputs {
            assert!(!contains_secret(input), "False positive on: {}", input);
        }
    }

    // ── tokenize_secrets tests ──
    //
    // These need a global cache installed. The fixture uses a small
    // hand-rolled in-memory map (not the production cache, to avoid
    // pulling hub into the common test build). Tests are serial because
    // the global slot is process-wide.

    use crate::secret_marker::{
        clear_global_cache, parse_markers, set_global_cache, ConsumedSecret, SecretCache,
    };
    use std::sync::{Arc, Mutex as StdMutex};

    /// Minimal cache impl for the tests — single-threaded enough for
    /// our purposes; mirrors the trait surface so the tokenizer
    /// exercises the real boundary.
    #[derive(Default)]
    struct ProbeCache {
        inner: StdMutex<Vec<(String, MarkerId, String, String, SecretSource)>>,
    }
    impl SecretCache for ProbeCache {
        fn capture(
            &self,
            session_key: &str,
            cleartext: Secret<String>,
            label: &str,
            source: SecretSource,
        ) -> MarkerId {
            let id = MarkerId::derived(session_key, cleartext.expose_secret());
            self.inner.lock().unwrap().push((
                session_key.to_string(),
                id.clone(),
                cleartext.expose_secret().clone(),
                label.to_string(),
                source,
            ));
            id
        }
        fn capture_named(&self, _: &str, _: &MarkerId, _: Secret<String>, _: SecretSource) {}
        fn list_named(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn lookup(&self, _: &str, _: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)> {
            None
        }
        fn purge_session(&self, _: &str) {}
        fn forget(&self, _: &str, _: &MarkerId) {}
    }

    /// `cargo test` runs tests in parallel within a binary; the global
    /// cache slot is shared. Serialize the tokenize tests via this
    /// mutex so they don't trample each other's installed cache.
    static TOKENIZE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn slack_source() -> SecretSource {
        SecretSource::Slack {
            workspace_id: "T1".into(),
            channel: "C1".into(),
            message_ts: "1.0".into(),
            user: "U1".into(),
        }
    }

    #[test]
    fn repeated_pastes_of_one_value_share_one_marker() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let key = concat!("sk-ant", "-api03-abcdefghijklmnopqrstuvwxyz1234567890");
        let input = format!("first {key} and again {key} at the end");
        let (out, captured) = tokenize_secrets(&input, "sess-1", slack_source());

        assert_eq!(captured.len(), 1, "one value, one capture");
        assert!(!out.contains("sk-ant-api03-abcdef"));
        let markers = parse_markers(&out);
        assert_eq!(markers.len(), 2);
        assert_eq!(
            markers[0], markers[1],
            "both occurrences render the same marker"
        );
        assert_eq!(markers[0].id, captured[0].id);

        clear_global_cache();
    }

    #[test]
    fn a_secret_that_prefixes_a_longer_one_leaves_the_longer_whole() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let short = concat!("sk-ant", "-api03-abcdefghijklmnopqrstuvwxyz");
        let long = format!("{short}0123456789");
        let input = format!("old {short} new {long}");
        let (out, captured) = tokenize_secrets(&input, "sess-1", slack_source());

        assert_eq!(captured.len(), 2);
        assert!(
            !out.contains("0123456789"),
            "the longer key's tail leaked: {out}"
        );
        let markers = parse_markers(&out);
        assert_eq!(markers.len(), 2);
        assert_ne!(markers[0].id, markers[1].id);
        assert_eq!(markers[1].preview, "sk-a...6789");

        clear_global_cache();
    }

    #[test]
    fn tokenize_replaces_anthropic_key_with_marker() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let input = concat!(
            "please save sk-ant",
            "-api03-abcdefghijklmnopqrstuvwxyz1234567890 for the github skill"
        );
        let (out, captured) = tokenize_secrets(input, "sess-1", slack_source());

        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].label, "ANTHROPIC_KEY");
        // Output is marker-substituted; cleartext is gone.
        assert!(!out.contains("sk-ant-api03-abcdef"));
        let markers = parse_markers(&out);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].label, "ANTHROPIC_KEY");
        assert_eq!(markers[0].id, captured[0].id);
        // Preview shape is `first4...last4`.
        assert_eq!(markers[0].preview, "sk-a...7890");

        clear_global_cache();
    }

    #[test]
    fn tokenize_handles_multiple_distinct_secrets() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let input = concat!(
            "anth=sk-ant",
            "-api03-abcdefghijklmnop1234567890 and gh=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"
        );
        let (out, captured) = tokenize_secrets(input, "sess-1", slack_source());

        assert_eq!(captured.len(), 2);
        let labels: Vec<_> = captured.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"ANTHROPIC_KEY"));
        assert!(labels.contains(&"GITHUB_PAT"));

        let markers = parse_markers(&out);
        assert_eq!(markers.len(), 2);
        assert!(!out.contains("sk-ant-api03-abcdef"));
        assert!(!out.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));

        clear_global_cache();
    }

    #[test]
    fn tokenize_no_secret_passes_through_unchanged() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let input = "ordinary message, no credentials, just words";
        let (out, captured) = tokenize_secrets(input, "sess-1", slack_source());
        assert_eq!(out, input);
        assert!(captured.is_empty());

        clear_global_cache();
    }

    #[test]
    fn tokenize_falls_back_to_redact_with_no_cache() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();

        let input = concat!(
            "leaked sk-ant",
            "-oat01-abcdefghijklmnopqrstuvwxyz1234567890 in transit"
        );
        let (out, captured) = tokenize_secrets(input, "sess-1", slack_source());

        // No cache → no markers, no captures, but cleartext still
        // sanitised to the legacy first6...last4 form.
        assert!(captured.is_empty());
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(out.contains("sk-ant...7890"));
        assert!(parse_markers(&out).is_empty());
    }

    // ── Vendor coverage matrix ──
    //
    // One row per catalogued vendor: a representative sample of the
    // public key format, and the expected label the redactor should
    // emit. Adding a new vendor in `LABELLED_PATTERNS` should be paired
    // with one row here. A regression on any existing pattern (e.g. a
    // widened character class swallowing a more specific format) shows
    // up as the wrong label rather than a silent miss.
    fn vendor_samples() -> Vec<(&'static str, &'static str)> {
        vec![
            // LLM providers
            (
                "ANTHROPIC_OAUTH_TOKEN",
                concat!("sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz1234567890"),
            ),
            (
                "ANTHROPIC_KEY",
                concat!("sk-ant", "-api03-abcdefghijklmnopqrstuvwxyz1234567890"),
            ),
            (
                "OPENAI_PROJECT_KEY",
                concat!("sk-pro", "j-abcdefghijklmnopqrstuvwxyz1234567890ABCDEF"),
            ),
            (
                "OPENROUTER_KEY",
                concat!("sk-or-", "v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"),
            ),
            (
                "OPENROUTER_LEGACY_KEY",
                "sk-or-abcdefghijklmnopqrstuvwxyz",
            ),
            ("OPENAI_KEY", "sk-abcdefghijklmnopqrstuvwxyz1234"),
            (
                "PERPLEXITY_KEY",
                "pplx-abcdefghijklmnopqrstuvwxyz1234567890ABCDEF",
            ),
            (
                "REPLICATE_KEY",
                concat!("r8_a", "bcdefghijklmnopqrstuvwxyz1234567890ABCDEF"),
            ),
            (
                "FIREWORKS_KEY",
                concat!("fw_a", "bcdefghijklmnopqrstuvwxyz1234567890ABCDEF"),
            ),
            (
                "HUGGINGFACE_KEY",
                concat!("hf_abc", "defghijklmnopqrstuvwxyz1234567890ABCDEF"),
            ),
            (
                "VOYAGE_KEY",
                concat!("pa-a", "bcdefghijklmnopqrstuvwxyz1234567890ABCDEF"),
            ),
            ("GROQ_KEY", concat!("gsk_", "abcdefghijklmnopqrst1234")),
            (
                "GOOGLE_API_KEY",
                concat!("AIzaSy", "A-abcdefghijklmnopqrstuvwxyz1234567890"),
            ),
            // Source control
            (
                "GITHUB_PAT",
                concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"),
            ),
            (
                "GITHUB_OAUTH_TOKEN",
                concat!("gho_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"),
            ),
            (
                "GITHUB_SERVER_TOKEN",
                concat!("ghs_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"),
            ),
            (
                "GITHUB_USER_TOKEN",
                concat!("ghu_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234"),
            ),
            (
                "GITHUB_PAT_FINE_GRAINED",
                concat!("gith", "ub_pat_abcdefghijklmnopqrstuvwxyz"),
            ),
            ("GITLAB_PAT", concat!("glpat-", "abcdefghijklmnopqrst1234")),
            // SaaS
            ("SLACK_TOKEN", concat!("xoxb-1", "234567890-abcdefghij-klmnopqrstuvwxyz")),
            ("SLACK_APP_TOKEN", concat!("xapp-1", "-A123-1234567890-abcdefghij")),
            ("LINEAR_API_TOKEN", concat!("lin_ap", "i_aBcDeFgHiJkLmNoPqRsT1234567890")),
            (
                "ATLASSIAN_API_TOKEN",
                concat!("ATAT", "T3xFfGF0abcdefghijklmnopqrstuvwxyz"),
            ),
            ("NOTION_TOKEN", concat!("ntn_", "abcdefghijklmnopqrstuvwxyz1234567890")),
            (
                "NOTION_LEGACY_TOKEN",
                concat!("secr", "et_abcdefghijklmnopqrstuvwxyzABCDEFGHIJabcdefghij1"),
            ),
            (
                "HUBSPOT_PAT",
                concat!("pat-na", "1-12345678-1234-1234-1234-1234567890ab"),
            ),
            (
                "DATADOG_APP_KEY",
                concat!("ddap", "p_abcdef0123456789abcdef0123456789abcdef01"),
            ),
            (
                "SENDGRID_KEY",
                concat!("SG.abc", "defghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyzABCDEFGHIJabcdefghij1234"),
            ),
            (
                "SENTRY_USER_TOKEN",
                concat!("sntr", "yu_abcdefghijklmnopqrstuvwxyzABCDEFGHIJabcdefghij1234567890abcdefghij"),
            ),
            (
                "SENTRY_ORG_TOKEN",
                concat!("sntr", "ys_abcdefghijklmnopqrstuvwxyzABCDEFGHIJabcdefghij1234567890abcdefghij"),
            ),
            (
                "TWILIO_ACCOUNT_SID",
                concat!("ACabcd", "ef0123456789abcdef0123456789"),
            ),
            (
                "DISCORD_BOT_TOKEN",
                concat!("MTIzND", "U2Nzg5MDEyMzQ1Njc4.GhIjKl.abcdefghijklmnopqrstuvwxyz1"),
            ),
            (
                "TELEGRAM_BOT_TOKEN",
                concat!("1234", "56789:AAAaaaBBBbbbCCCcccDDDdddEEEeeeFFFff"),
            ),
            (
                "ASANA_PAT",
                concat!("1/12", "34567890123456:abcdef0123456789abcdef0123456789"),
            ),
            (
                "DIGITALOCEAN_TOKEN",
                concat!("dop_v1", "_abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"),
            ),
            ("BRAVE_SEARCH_KEY", concat!("BSAa", "bcdefghij1234567890ABCDEFG")),
            (
                "WORKFORCE_PAIRING_TOKEN",
                concat!("wfp_", "abc123def456ghi789jkl012mno345pqr"),
            ),
            (
                "WORKFORCE_REMOTE_MCP_TOKEN",
                concat!("wfr_", "Qm9vayBvZiBzZWNyZXRzIGZvciB0ZXN0aW5n"),
            ),
            // Payments
            (
                "STRIPE_SECRET",
                concat!("sk_liv", "e_abcdef0123456789ABCDEFGHIJKLMN"),
            ),
            (
                "STRIPE_WEBHOOK_SECRET",
                concat!("whsec_", "abcdef0123456789ABCDEFGHIJabcdefgh"),
            ),
            // Cloud
            ("AWS_ACCESS_KEY", "AKIAIOSFODNN7EXAMPLE"),
            (
                "AGE_SECRET_KEY",
                concat!("AGE-", "SECRET-KEY-1ABCDEFGHIJKLMNOPQRSTUVWXYZ234567890ABCDEFGHIJKLMNO"),
            ),
            // Dev / CI registries
            ("NPM_TOKEN", concat!("npm_", "abcdefghijklmnopqrst1234")),
            (
                "DOCKER_HUB_PAT",
                concat!("dckr", "_pat_abcdefghijklmnopqrstuvwxyz1234567890"),
            ),
            (
                "PYPI_TOKEN",
                concat!("pypi-A", "gEIcHlwaS5vcmcabcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJabcdefghij"),
            ),
            (
                "RUBYGEMS_KEY",
                concat!("rubyge", "ms_abcdef0123456789abcdef0123456789abcdef0123456789"),
            ),
            // Generic
            (
                "JWT",
                concat!("eyJh", "bGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"),
            ),
            (
                "BASIC_AUTH",
                concat!("Basi", "c dXNlcjpwYXNzd29yZGFiY2RlZmdoaWprbA=="),
            ),
            (
                "BEARER_TOKEN",
                concat!("Bear", "er eyJhbGciOiJIUzI1NiJ9.payload.signature123456"),
            ),
        ]
    }

    #[test]
    fn every_catalogued_vendor_is_redacted() {
        for (label, sample) in vendor_samples() {
            assert!(
                contains_secret(sample),
                "{label}: sample `{sample}` was not detected by any pattern"
            );
            let redacted = redact_secrets(sample);
            assert_ne!(
                redacted, sample,
                "{label}: sample `{sample}` was not redacted"
            );
        }
    }

    #[test]
    fn every_catalogued_vendor_emits_correct_label() {
        // Walk the catalogue in priority order and find the first
        // pattern that matches each sample — the same logic
        // `tokenize_secrets` runs internally, but without the global
        // cache so this test doesn't race with other modules' fixtures.
        for (expected_label, sample) in vendor_samples() {
            let matched = LABELLED_PATTERNS.iter().find_map(|p| {
                p.regex
                    .find(sample)
                    .filter(|m| !is_allowlisted(m.as_str()))
                    .map(|_| p.label)
            });
            assert_eq!(
                matched,
                Some(expected_label),
                "{expected_label}: sample `{sample}` was matched by `{:?}` (priority order)",
                matched
            );
        }
    }

    #[test]
    fn no_false_positives_on_common_non_secrets() {
        // Hashes / IDs / blobs that look credential-shaped but aren't.
        let inputs = &[
            // SHA-256 of "hello" — 64 hex chars
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            // SHA-1 commit hash — 40 hex chars
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
            // UUID
            "550e8400-e29b-41d4-a716-446655440000",
            // Slack channel ID
            "C012AB3CD",
            // Slack message timestamp
            "1234567890.123456",
            // base64 blob
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg==",
            // Stripe publishable (allowlisted, public)
            "pk_live_abcdef0123456789ABCDEFGHIJKLMN",
            "pk_test_abcdef0123456789ABCDEFGHIJKLMN",
            // ordinary prose mentioning credential-like words
            "The BSA meeting is scheduled for Tuesday",
            "Please review the linear algebra homework",
            "The age of the server is 42 days",
            "Build Status: All checks passed",
            "Running integration tests on wfp module",
        ];
        for input in inputs {
            assert!(!contains_secret(input), "False positive on: {input}");
            assert_eq!(
                redact_secrets(input),
                *input,
                "Unexpected mutation of: {input}"
            );
        }
    }

    #[test]
    fn stripe_publishable_key_is_allowlisted() {
        let input = "STRIPE_PUBLISHABLE_KEY=pk_live_abcdef0123456789ABCDEFGHIJKLMN";
        // The widened ENV_CREDENTIAL would otherwise capture this. The
        // allowlist short-circuits both detection and redaction.
        assert!(!contains_secret(input));
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn openrouter_key_is_redacted_not_swallowed_by_openai_pattern() {
        // The OPENAI_KEY pattern can't match because its character class
        // excludes hyphens — this test is the regression guard against
        // a future maintainer "widening" OPENAI_KEY and breaking us.
        let input = concat!(
            "Here is my key: sk-or-",
            "v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        let (out, captured) = tokenize_secrets(input, "sess-or", slack_source());
        assert_eq!(captured.len(), 1, "expected exactly one capture");
        assert_eq!(captured[0].label, "OPENROUTER_KEY");
        assert!(!out.contains("abcdef0123456789"));

        clear_global_cache();
    }

    #[test]
    fn widened_env_credential_does_not_match_inside_compound_words() {
        // `foreign_key`, `cache_key`, `partition_key`, `idempotency_key`,
        // `api_secret_id` etc. should NOT be treated as credentials —
        // they're just identifiers that happen to contain a keyword.
        // Underscores are word characters, so `\b`-anchored keyword
        // alternatives shouldn't match the `key` inside `foreign_key`.
        let inputs = &[
            r#"{"foreign_key": "user_profile_lookup_abcdef"}"#,
            "cache_key: products_index_lookup_alpha_v2",
            "partition_key=customer_segments_north_america_q4",
            "idempotency_key: order_2026_05_22_user_42_v1",
            "primary_key=widget_inventory_table_main_idx",
        ];
        for input in inputs {
            assert!(
                !contains_secret(input),
                "False positive on identifier-containing-keyword: {input}"
            );
            assert_eq!(
                redact_secrets(input),
                *input,
                "Unexpected mutation of: {input}"
            );
        }
    }

    #[test]
    fn widened_env_credential_catches_lowercase_hyphenated() {
        // Previously these slipped through (uppercase-only, no hyphen).
        let inputs = &[
            "api_key=abcdefghijklmnopqrst1234",
            "api-key: abcdefghijklmnopqrst1234",
            "access_token = 'abcdefghijklmnopqrst1234'",
            "client_secret: \"abcdefghijklmnopqrst1234\"",
        ];
        for input in inputs {
            assert!(
                contains_secret(input),
                "Widened ENV_CREDENTIAL missed: {input}"
            );
        }
    }

    #[test]
    fn tokenize_specific_label_wins_over_generic() {
        let _g = TOKENIZE_TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache: Arc<dyn SecretCache> = Arc::new(ProbeCache::default());
        set_global_cache(Arc::clone(&cache));

        // Three nested prefixes: sk-ant-oat ⊂ sk-ant- ⊂ sk-. The most
        // specific label must win, so an OAuth token is ANTHROPIC_OAUTH_TOKEN
        // (not ANTHROPIC_KEY, and not OPENAI_KEY), and the generic patterns
        // don't double-match the same span.
        let input = concat!(
            "key sk-ant",
            "-oat01-abcdefghijklmnopqrstuvwxyz1234567890 done"
        );
        let (_out, captured) = tokenize_secrets(input, "sess-1", slack_source());
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].label, "ANTHROPIC_OAUTH_TOKEN");

        clear_global_cache();
    }
}
