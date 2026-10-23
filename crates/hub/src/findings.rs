//! `report_findings` — the tool a worker sub-agent calls to record structured
//! research findings. The tool itself only validates and acknowledges; the
//! findings are collated from the run's recorded tool invocations by
//! [`collate_findings`], which [`crate::serve::AgentService::run_isolated`]
//! calls once per worker run: they travel on as structured [`Finding`]s (what
//! `investigate` adjudicates) and are staged as one tabular artifact (what a
//! spawning agent `data.query`s).
//!
//! Every finding carries a 0–100 confidence and its citations. Parsing is per
//! element and fail-safe, like claim grounding's: a malformed finding is
//! rejected on its own, and an unreadable confidence or verdict reads as the
//! weakest value, never a strong one.

use std::time::Instant;

use async_trait::async_trait;
use eyre::{bail, Result};
use serde_json::{json, Value};
use tracing::warn;
use workforce_agent::artifacts::{stage_table, staged_to_output, ArtifactStore};
use workforce_agent::runner::ToolInvocation;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::text::truncate_with_ellipsis;

use crate::claim_verifier::{RawTier, Tier};

/// The citation forms [`citation`] accepts, worded once for every prompt and
/// refusal that states them.
macro_rules! citation_forms {
    () => {
        "a file location (`src/lib.rs:42`, `src/lib.rs:42-60`, `src/lib.rs#L42`, or a bare \
         `src/lib.rs`), a URL, a ticket key (`ABC-123`), a reference such as \
         `metabase card 42`, `dashboard:12` or `memory:<id>`, or `tool:<tool name> <what you \
         asked it>` for anything else a tool returned"
    };
}
pub(crate) use crate::hub_core::jobs::CITATIONS_KEPT;
pub use crate::hub_core::jobs::{Challenge, Finding};
pub(crate) use citation_forms;

/// Tabular columns for a staged findings artifact.
const FINDINGS_COLUMNS: &[&str] = &[
    "id",
    "claim",
    "confidence",
    "citations",
    "evidence",
    "tags",
    "target",
    "verdict",
];

/// Producer/source label for staged findings artifacts.
pub(crate) const FINDINGS_PRODUCER: &str = "subagent.findings";

/// Tool name. A bare (dot-free) name so the registry key and the LLM-facing
/// name are identical — collation filters invocations by this exact name.
const REPORT_FINDINGS_TOOL: &str = "report_findings";

/// What a `report_findings` call recorded, and what it could not.
#[derive(Debug, Default)]
struct Parsed {
    findings: Vec<Finding>,
    rejected: Vec<Value>,
    warnings: Vec<String>,
}

/// Parse a `report_findings` input element by element. Errs only when there
/// is no `findings` array to read at all.
fn parse_findings(input: &Value) -> Result<Parsed> {
    let items = input
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| eyre::eyre!("'findings' must be an array"))?;
    if items.is_empty() {
        bail!("'findings' must not be empty");
    }
    Ok(items
        .iter()
        .enumerate()
        .fold(Parsed::default(), |mut parsed, (i, item)| {
            let n = i + 1;
            match parse_finding(item) {
                Ok((finding, warnings)) => {
                    parsed.findings.push(finding);
                    parsed
                        .warnings
                        .extend(warnings.into_iter().map(|w| format!("finding {n}: {w}")));
                }
                Err(reason) => parsed
                    .rejected
                    .push(json!({ "finding": n, "reason": reason })),
            }
            parsed
        }))
}

fn parse_finding(item: &Value) -> std::result::Result<(Finding, Vec<String>), String> {
    let claim = item
        .get("claim")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("needs a non-empty 'claim'")?;
    let mut warnings = Vec::new();
    let confidence = item.get("confidence").and_then(parse_confidence);
    if confidence.is_none() {
        warnings.push("no confidence on the 0–100 scale; recorded as the lowest".to_string());
    }
    let (citations, refused): (Vec<_>, Vec<_>) = strings(item.get("citations"))
        .into_iter()
        .map(|raw| citation(&raw).ok_or(raw))
        .partition(std::result::Result::is_ok);
    let citations: Vec<String> = citations
        .into_iter()
        .flatten()
        .take(CITATIONS_KEPT)
        .collect();
    warnings.extend(refused.into_iter().filter_map(|r| r.err()).map(|raw| {
        format!(
            "'{}' is not a citation; use {}",
            truncate_with_ellipsis(&raw, 120),
            citation_forms!()
        )
    }));
    if citations.is_empty() {
        warnings.push(
            "no usable citation, so it is held back as uncited, not reported as fact".to_string(),
        );
    }
    let challenge = item
        .get("target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|target| Challenge {
            target: target_id(target),
            verdict: item
                .get("verdict")
                .and_then(|v| serde_json::from_value::<RawTier>(v.clone()).ok())
                .unwrap_or(RawTier::Unknown)
                .into(),
        });
    Ok((
        Finding {
            claim: truncate_with_ellipsis(claim, FIELD_MAX_BYTES),
            confidence,
            citations,
            evidence: truncate_with_ellipsis(
                item.get("evidence")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim(),
                FIELD_MAX_BYTES,
            ),
            tags: strings(item.get("tags")),
            challenge,
        },
        warnings,
    ))
}

/// Bounds on what one finding carries, since every finding is persisted with
/// its job and read back into later steps' prompts.
const FIELD_MAX_BYTES: usize = 1_000;

/// A challenger's `target` as the finding id it names: `f3` from `f3`, `[f3]`,
/// `F3` or `w0.f3`.
fn target_id(target: &str) -> String {
    let bare = target.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace());
    bare.rsplit('.').next().unwrap_or(bare).to_ascii_lowercase()
}

/// A 0–100 confidence from a number or a numeric string; anything else —
/// including a value off the scale — is no confidence at all.
fn parse_confidence(value: &Value) -> Option<u8> {
    let n = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().trim_end_matches('%').parse().ok()?,
        _ => return None,
    };
    (0.0..=100.0).contains(&n).then(|| n.round() as u8)
}

/// A string or an array of strings, trimmed, empties dropped.
fn strings(value: Option<&Value>) -> Vec<String> {
    let items: Vec<&Value> = match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(single) => vec![single],
        None => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Words that may name what a reference points into: `jira:ABC-1`,
/// `metabase card 42`, `memory:3f2a…`.
const REFERENCE_WORDS: &[&str] = &[
    "card",
    "collection",
    "commit",
    "confluence",
    "dashboard",
    "datadog",
    "doc",
    "github",
    "issue",
    "jira",
    "knowledge",
    "linear",
    "log",
    "logs",
    "memory",
    "metabase",
    "monitor",
    "notes",
    "notion",
    "pr",
    "question",
    "session",
    "sessions",
    "slack",
    "table",
    "ticket",
];

/// Slash-joined words that read as prose, not paths.
const NOT_PATHS: &[&str] = &[
    "and/or",
    "n/a",
    "w/o",
    "i/o",
    "y/n",
    "yes/no",
    "either/or",
    "on/off",
    "read/write",
    "true/false",
];

/// Uppercase-dash-number tokens that name a standard or a model, not a ticket.
const NOT_TICKET_PREFIXES: &[&str] = &["AES", "GPT", "ISO", "MD", "RSA", "SHA", "TLS", "UTF"];

/// The locator `raw` cites, normalised, or `None` when it locates nothing.
///
/// A citation must say where its evidence can be found again. Workers cite
/// files, the web, tickets, dashboards, memory and whatever else their read
/// tools returned, in many spellings — wrapped in backticks or a Markdown
/// link, followed by a description — so the forms are generous. Prose that
/// names no place ("the docs", "my knowledge", "see above") is refused, and so
/// is a finding id: a challenger citing the finding it judges shows nothing.
fn citation(raw: &str) -> Option<String> {
    let s = unwrap(raw);
    if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("tool:")) {
        return tool_reference(&s[5..]).then(|| format!("tool:{}", s[5..].trim()));
    }
    let tokens: Vec<String> = s
        .split_whitespace()
        .map(unwrap)
        .filter(|t| !t.is_empty())
        .collect();
    let (head, rest) = tokens.split_first()?;
    (is_locator(head) || is_reference(head, rest) || (is_tool_name(head) && !rest.is_empty()))
        .then(|| tokens.join(" "))
}

/// `jira.get_issue`, `metabase.card_query`: a dotted tool name, which
/// followed by what was asked of it (`jira.get_issue ABC-12`) is the `tool:`
/// form without its prefix.
fn is_tool_name(token: &str) -> bool {
    let mut parts = token.split('.');
    let word = |p: &str| {
        p.starts_with(|c: char| c.is_ascii_lowercase())
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    parts.next().is_some_and(word) && token.contains('.') && parts.all(word)
}

/// `s` without the wrapping a model puts round a locator: whitespace, quotes,
/// backticks, angle brackets, a Markdown link's text, trailing punctuation.
fn unwrap(raw: &str) -> String {
    let s = raw.trim();
    if let Some(url) = s
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]("))
        .and_then(|(_, rest)| rest.strip_suffix(')'))
    {
        return unwrap(url);
    }
    let trimmed = s
        .trim_matches(|c: char| "`'\"<>".contains(c))
        .trim_end_matches([',', ';', '.'])
        .trim();
    let trimmed = unbalanced_parens(trimmed);
    if trimmed.len() == s.len() {
        trimmed.to_string()
    } else {
        unwrap(trimmed)
    }
}

/// `s` without a leading `(` or trailing `)` it does not close or open, so
/// `(src/a.rs:10)` loses its brackets and `…/wiki/Foo_(bar)` keeps its own.
fn unbalanced_parens(s: &str) -> &str {
    if let Some(inner) = s.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        if !inner.contains(['(', ')']) {
            return inner;
        }
    }
    match s.matches('(').count().cmp(&s.matches(')').count()) {
        std::cmp::Ordering::Greater => s.strip_prefix('(').unwrap_or(s),
        std::cmp::Ordering::Less => s.strip_suffix(')').unwrap_or(s),
        std::cmp::Ordering::Equal => s,
    }
}

/// `<tool name> <what>` after a `tool:` prefix.
fn tool_reference(rest: &str) -> bool {
    rest.trim()
        .split_once(char::is_whitespace)
        .is_some_and(|(name, what)| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                && !what.trim().is_empty()
        })
}

/// A single token that locates something on its own.
fn is_locator(token: &str) -> bool {
    is_url(token)
        || is_namespaced(token)
        || is_ticket_key(token)
        || is_commit(token)
        || (!is_finding_id(token) && is_path(token))
}

fn is_url(token: &str) -> bool {
    token.split_once("://").is_some_and(|(scheme, rest)| {
        scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            && !rest.is_empty()
    })
}

/// `jira:ABC-12`, `dashboard:12`, `memory:3f2a`.
fn is_namespaced(token: &str) -> bool {
    token.split_once(':').is_some_and(|(word, rest)| {
        is_reference_word(word) && !rest.is_empty() && !rest.starts_with("//")
    })
}

fn is_reference_word(word: &str) -> bool {
    REFERENCE_WORDS.contains(&word.to_ascii_lowercase().as_str())
}

/// `metabase card 42`, `jira ABC-12`, `PR #123`: up to two reference words,
/// then an id with a digit in it (or a locator).
fn is_reference(head: &str, rest: &[String]) -> bool {
    let word = |t: &str| is_reference_word(t.trim_end_matches(':'));
    let id = |t: &str| {
        let t = t.trim_start_matches(['#', ':']);
        t.chars().any(|c| c.is_ascii_digit()) || is_locator(t)
    };
    word(head)
        && match rest {
            [next, ..] if id(next) => true,
            [next, then, ..] => word(next) && id(then),
            _ => false,
        }
}

/// `ABC-123`, but not `UTF-8` or `SHA-256`.
fn is_ticket_key(token: &str) -> bool {
    token.split_once('-').is_some_and(|(key, number)| {
        (2..=10).contains(&key.len())
            && key.starts_with(|c: char| c.is_ascii_uppercase())
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && !NOT_TICKET_PREFIXES.contains(&key)
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
    })
}

/// A 7–40 character hex commit id, with both a digit and a letter so neither
/// a number nor a word like `facade` passes.
fn is_commit(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_hexdigit())
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

/// `f3` or `w0.f3`: an id this research chain gave a finding.
fn is_finding_id(token: &str) -> bool {
    let numbered = |s: &str, prefixes: &[char]| {
        s.len() > 1 && s.starts_with(prefixes) && s[1..].chars().all(|c| c.is_ascii_digit())
    };
    let token = token.to_ascii_lowercase();
    match token.split_once('.') {
        None => numbered(&token, &['f']),
        Some((step, id)) => numbered(step, &['w', 'c']) && numbered(id, &['f']),
    }
}

/// A repository path or file name, optionally with a line anchor:
/// `src/a.rs`, `README.md`, `src/a.rs:12`, `:12-40`, `:12:5`, `:L12`,
/// `#L12`, `#L12-L40`, `owner/repo#123`.
fn is_path(token: &str) -> bool {
    let path = strip_anchor(token);
    if path.is_empty()
        || !path
            .chars()
            .all(|c| c.is_alphanumeric() || "/._-~@+".contains(c))
        || NOT_PATHS.contains(&path.to_ascii_lowercase().as_str())
    {
        return false;
    }
    let anchored = path.len() < token.len();
    let named = |name: &str| {
        name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 10
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
                && (stem.trim_start_matches('.').len() > 1 || ext.len() > 1)
        })
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [] => false,
        [one] if !path.contains('/') => anchored || named(one),
        _ => true,
    }
}

/// `token` without a trailing line or item anchor, when it has one.
fn strip_anchor(token: &str) -> &str {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let line = |s: &str| digits(s.strip_prefix(['L', 'l']).unwrap_or(s));
    let range = |s: &str| match s.split_once('-') {
        Some((a, b)) => line(a) && line(b),
        None => line(s),
    };
    if let Some((path, anchor)) = token.rsplit_once('#') {
        if range(anchor) {
            return path;
        }
    }
    let mut path = token;
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((head, anchor)) if range(anchor) => path = head,
            _ => break,
        }
    }
    path
}

/// A worker run's findings: the structured set, and its staged artifact.
#[derive(Debug, Default)]
pub struct Collated {
    pub findings: Vec<Finding>,
    /// The model-facing envelope of the staged artifact; `None` when nothing
    /// was recorded or staging failed (the structured set still stands).
    pub staged: Option<Value>,
}

/// Collate findings recorded via `report_findings` across a run's tool
/// invocations, element by element, and stage them as one tabular artifact.
/// Staged with the worker's (detached, session-less) context, so the artifact
/// is addressable by its handle across sessions, which is how the spawning
/// agent reaches it.
pub fn collate_findings(
    store: Option<&dyn ArtifactStore>,
    agent_id: &str,
    invocations: &[ToolInvocation],
) -> Collated {
    let findings: Vec<Finding> = invocations
        .iter()
        .filter(|i| i.tool_name == REPORT_FINDINGS_TOOL)
        .filter_map(|i| parse_findings(&i.input).ok())
        .flat_map(|parsed| parsed.findings)
        .fold(Vec::new(), |mut kept: Vec<Finding>, finding| {
            match kept.iter_mut().find(|k| k.same_as(&finding)) {
                Some(earlier) => earlier.restate(finding),
                None => kept.push(finding),
            }
            kept
        });
    if findings.is_empty() {
        return Collated::default();
    }
    let rows: Vec<Value> = findings
        .iter()
        .enumerate()
        .map(|(i, f)| finding_row(&finding_id(i), f))
        .collect();
    let columns = FINDINGS_COLUMNS.iter().map(|s| s.to_string()).collect();
    // Always stage (threshold 0) so findings are uniformly addressable by a
    // handle, never only through context.
    let staged = store.and_then(|store| {
        match stage_table(store, agent_id, FINDINGS_PRODUCER, Some(columns), rows, 0) {
            Ok(staged) => Some(staged_to_output(staged, FINDINGS_PRODUCER)),
            Err(e) => {
                warn!(error = %e, "Failed to stage worker findings");
                None
            }
        }
    });
    Collated { findings, staged }
}

/// The id a finding is known by within its run: `f1`, `f2`, … in the order
/// recorded. A challenger names its target by this id.
pub fn finding_id(index: usize) -> String {
    format!("f{}", index + 1)
}

fn finding_row(id: &str, f: &Finding) -> Value {
    json!({
        "id": id,
        "claim": f.claim,
        "confidence": f.confidence,
        "citations": f.citations.join("; "),
        "evidence": f.evidence,
        "tags": f.tags.join(";"),
        "target": f.challenge.as_ref().map(|c| c.target.as_str()).unwrap_or_default(),
        "verdict": f.challenge.as_ref().map(|c| tier_name(c.verdict)).unwrap_or_default(),
    })
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Verified => "verified",
        Tier::Contradicted => "contradicted",
        Tier::Unverified => "unverified",
    }
}

/// Worker-only tool that records structured research findings.
pub struct ReportFindingsTool;

#[async_trait]
impl Tool for ReportFindingsTool {
    fn name(&self) -> &str {
        REPORT_FINDINGS_TOOL
    }

    fn description(&self) -> &str {
        concat!(
            "Record structured research findings as you investigate. Each finding is a claim, \
             your confidence from 0 to 100, and its citations. A citation is ",
            citation_forms!(),
            ". A finding without one is held back as uncited, not reported as fact; recording \
             the same claim again with a citation replaces it. When you are challenging another \
             worker's findings, also set `target` (the finding's id) and `verdict`."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The findings to record.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "claim": { "type": "string", "description": "The assertion." },
                            "confidence": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 100,
                                "description": "How sure the evidence makes you: 90+ read it directly, 60–89 strong inference, below 60 a lead to check."
                            },
                            "citations": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": concat!("Where it came from, each one ", citation_forms!(), ".")
                            },
                            "evidence": { "type": "string", "description": "Quote, snippet, or data backing the claim." },
                            "tags": { "type": "array", "items": { "type": "string" }, "description": "Theme buckets for grouping." },
                            "target": { "type": "string", "description": "Challengers only: the id of the finding this verdict is about (e.g. f3)." },
                            "verdict": {
                                "type": "string",
                                "enum": ["verified", "contradicted", "unverified"],
                                "description": "Challengers only: verified = it holds, contradicted = you found it wrong, unverified = you could not settle it."
                            }
                        },
                        "required": ["claim", "confidence", "citations"]
                    }
                }
            },
            "required": ["findings"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let parsed = match parse_findings(&input) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid findings: {e}"),
                    elapsed(),
                ))
            }
        };
        if parsed.findings.is_empty() {
            return Ok(ToolResult::failure(
                format!("No finding recorded: {}", Value::Array(parsed.rejected)),
                elapsed(),
            ));
        }
        let recorded = parsed.findings.len();
        let mut ack = json!({ "recorded": recorded });
        let uncited = parsed
            .findings
            .iter()
            .filter(|f| f.citations.is_empty())
            .count();
        if uncited > 0 {
            ack["action_needed"] = json!(format!(
                "{uncited} of {recorded} finding(s) have no usable citation and will be held \
                 back as uncited. Record each again with the same claim and a citation — {} — \
                 and it replaces the uncited one.",
                citation_forms!()
            ));
        }
        if !parsed.rejected.is_empty() {
            ack["rejected"] = Value::Array(parsed.rejected);
        }
        if !parsed.warnings.is_empty() {
            ack["warnings"] = json!(parsed.warnings);
        }
        Ok(ToolResult::success(ack, elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cited(claim: &str) -> Value {
        json!({ "claim": claim, "confidence": 80, "citations": ["src/a.rs:10"] })
    }

    #[test]
    fn parse_findings_reads_confidence_citations_and_tags() {
        let parsed = parse_findings(&json!({
            "findings": [{
                "claim": "  X is true ",
                "confidence": 85,
                "citations": ["src/lib.rs:12-20", "https://example.com/doc"],
                "evidence": "e",
                "tags": ["a", "b"]
            }]
        }))
        .unwrap();
        assert!(parsed.rejected.is_empty() && parsed.warnings.is_empty());
        let f = &parsed.findings[0];
        assert_eq!(f.claim, "X is true");
        assert_eq!(f.confidence, Some(85));
        assert_eq!(f.citations, ["src/lib.rs:12-20", "https://example.com/doc"]);
        assert_eq!(f.tags, ["a", "b"]);
        assert!(f.challenge.is_none());
    }

    /// One bad element is rejected on its own; the rest of the call stands.
    #[test]
    fn a_malformed_finding_does_not_void_the_call() {
        let parsed = parse_findings(&json!({
            "findings": [cited("a"), { "evidence": "no claim" }, 42, { "claim": "   " }, cited("b")]
        }))
        .unwrap();
        let claims: Vec<&str> = parsed.findings.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, ["a", "b"]);
        let rejected: Vec<u64> = parsed
            .rejected
            .iter()
            .map(|r| r["finding"].as_u64().unwrap())
            .collect();
        assert_eq!(rejected, [2, 3, 4]);
    }

    /// An unreadable confidence reads as the weakest, never a strong one.
    #[test]
    fn confidence_fails_safe_to_none() {
        let conf = |v: Value| parse_confidence(&v);
        assert_eq!(conf(json!(70)), Some(70));
        assert_eq!(conf(json!(69.6)), Some(70));
        assert_eq!(conf(json!("55")), Some(55));
        assert_eq!(conf(json!("90%")), Some(90));
        assert_eq!(conf(json!(150)), None);
        assert_eq!(conf(json!(-1)), None);
        assert_eq!(conf(json!("high")), None);
        assert_eq!(conf(json!(null)), None);

        let parsed =
            parse_findings(&json!({ "findings": [{ "claim": "x", "confidence": "high", "citations": ["a.rs:1"] }] }))
                .unwrap();
        assert_eq!(parsed.findings[0].confidence, None);
        assert!(parsed.warnings[0].contains("confidence"));
    }

    #[test]
    fn every_genuine_locator_is_a_citation_and_prose_is_not() {
        for (raw, kept) in [
            ("src/a.rs:10", "src/a.rs:10"),
            ("app/models/order.rb:12-40", "app/models/order.rb:12-40"),
            ("src/a.rs:12:5", "src/a.rs:12:5"),
            ("src/a.rs:L42", "src/a.rs:L42"),
            ("src/a.rs#L10", "src/a.rs#L10"),
            ("src/a.rs#L10-L20", "src/a.rs#L10-L20"),
            ("README.md:3", "README.md:3"),
            ("crates/hub/src/serve.rs", "crates/hub/src/serve.rs"),
            ("Cargo.toml", "Cargo.toml"),
            ("crates/hub/", "crates/hub/"),
            ("`src/a.rs:10` — the handler", "src/a.rs:10 — the handler"),
            ("src/a.rs:10.", "src/a.rs:10"),
            ("https://example.com/x?y=1", "https://example.com/x?y=1"),
            ("<https://example.com/x>", "https://example.com/x"),
            (
                "[the runbook](https://example.com/run)",
                "https://example.com/run",
            ),
            ("s3://bucket/key.csv", "s3://bucket/key.csv"),
            (
                "https://example.com/x (release notes)",
                "https://example.com/x release notes",
            ),
            ("tool:jira.get_issue ABC-12", "tool:jira.get_issue ABC-12"),
            (
                "tool:metabase.card_query card 42",
                "tool:metabase.card_query card 42",
            ),
            (
                "Tool: memory recall cache ttl",
                "tool:memory recall cache ttl",
            ),
            ("ABC-123", "ABC-123"),
            ("jira:ABC-12", "jira:ABC-12"),
            ("jira ABC-12", "jira ABC-12"),
            ("metabase card 42", "metabase card 42"),
            ("Metabase dashboard #12", "Metabase dashboard #12"),
            ("dashboard:12", "dashboard:12"),
            ("card 42", "card 42"),
            ("memory:3f2a9c", "memory:3f2a9c"),
            ("knowledge:auth-overview", "knowledge:auth-overview"),
            ("owner/repo#123", "owner/repo#123"),
            ("PR #123", "PR #123"),
            ("commit 4be7f1a", "commit 4be7f1a"),
            ("4be7f1a9", "4be7f1a9"),
            ("jira.get_issue ABC-12", "jira.get_issue ABC-12"),
            (
                "datadog.logs_search service:api",
                "datadog.logs_search service:api",
            ),
            ("(src/a.rs:10)", "src/a.rs:10"),
            (
                "https://en.wikipedia.org/wiki/Foo_(bar)",
                "https://en.wikipedia.org/wiki/Foo_(bar)",
            ),
        ] {
            assert_eq!(citation(raw).as_deref(), Some(kept), "{raw}");
        }
        for prose in [
            "my knowledge",
            "the docs",
            "see above",
            "memory",
            "knowledge base",
            "none",
            "N/A",
            "and/or",
            "e.g.",
            "etc.",
            "UTF-8",
            "SHA-256",
            "GPT-4",
            "ISO-8601",
            "facade",
            "1234",
            "f1",
            "w0.f1",
            "[f2]",
            "src/a.rs:ten",
            "a.rs:",
            ":10",
            "https://",
            "tool:jira.get_issue",
            "metabase.card_query",
            "tool: ",
            "",
        ] {
            assert_eq!(citation(prose), None, "{prose}");
        }

        let parsed = parse_findings(&json!({
            "findings": [{ "claim": "x", "confidence": 90, "citations": ["the docs", "a.rs:4"] },
                         { "claim": "y", "confidence": 90, "citations": "nowhere" }]
        }))
        .unwrap();
        assert_eq!(parsed.findings[0].citations, ["a.rs:4"]);
        assert!(parsed.findings[1].citations.is_empty());
        assert!(parsed.warnings.iter().any(|w| w.contains("uncited")));
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w
                    .starts_with("finding 1: 'the docs' is not a citation; use a file location")),
            "the refusal tells the worker what would count: {:?}",
            parsed.warnings
        );
    }

    /// A worker that fixes a refused citation re-records the claim; the
    /// latest record takes the uncited one's place rather than sitting beside
    /// it, and restating a cited claim never costs it its citations.
    #[test]
    fn re_recording_a_claim_restates_it() {
        let invocations = vec![
            invocation(
                "report_findings",
                json!({ "findings": [
                    { "claim": "TTL is 60s", "confidence": 80, "citations": ["the docs"] },
                    cited("other")
                ] }),
            ),
            invocation(
                "report_findings",
                json!({ "findings": [
                    { "claim": "ttl is  60s", "confidence": 85, "citations": ["src/cache.rs:40"] },
                    { "claim": "TTL is 60s", "confidence": 90, "citations": ["b.rs:1"], "target": "f1", "verdict": "verified" }
                ] }),
            ),
        ];
        let collated = collate_findings(None, "agent", &invocations);
        let kept: Vec<(&str, &[String], bool)> = collated
            .findings
            .iter()
            .map(|f| {
                (
                    f.claim.as_str(),
                    f.citations.as_slice(),
                    f.challenge.is_some(),
                )
            })
            .collect();
        assert_eq!(
            kept,
            [
                ("ttl is  60s", &["src/cache.rs:40".to_string()][..], false),
                ("other", &["src/a.rs:10".to_string()][..], false),
                ("TTL is 60s", &["b.rs:1".to_string()][..], true),
            ],
            "replaced in place, so ids stay stable; a verdict is not the claim it judges"
        );

        let restated = collate_findings(
            None,
            "agent",
            &[
                invocation("report_findings", json!({ "findings": [cited("x")] })),
                invocation(
                    "report_findings",
                    json!({ "findings": [{ "claim": "x", "confidence": 95, "citations": [] }] }),
                ),
            ],
        );
        assert_eq!(restated.findings.len(), 1);
        assert_eq!(restated.findings[0].confidence, Some(95));
        assert_eq!(restated.findings[0].citations, ["src/a.rs:10"]);
    }

    /// A challenge reuses claim grounding's tiers, and a garbled verdict fails
    /// safe to unverified — never a silent confirmation.
    #[test]
    fn a_challenge_carries_its_target_and_a_fail_safe_verdict() {
        let parsed = parse_findings(&json!({
            "findings": [
                { "claim": "f1 is wrong", "confidence": 90, "citations": ["b.rs:2"], "target": "f1", "verdict": "contradicted" },
                { "claim": "f2 holds", "confidence": 90, "citations": ["b.rs:3"], "target": "f2", "verdict": "Confirmed!" },
                { "claim": "f3 unsure", "confidence": 90, "citations": ["b.rs:4"], "target": "f3" },
                { "claim": "f4 holds", "confidence": 90, "citations": ["b.rs:5"], "target": "[W0.F4]", "verdict": "verified" }
            ]
        }))
        .unwrap();
        let verdicts: Vec<(String, Tier)> = parsed
            .findings
            .into_iter()
            .map(|f| f.challenge.map(|c| (c.target, c.verdict)).unwrap())
            .collect();
        assert_eq!(
            verdicts,
            [
                ("f1".to_string(), Tier::Contradicted),
                ("f2".to_string(), Tier::Unverified),
                ("f3".to_string(), Tier::Unverified),
                ("f4".to_string(), Tier::Verified),
            ]
        );
    }

    #[test]
    fn parse_findings_rejects_a_missing_or_empty_array() {
        assert!(parse_findings(&json!({ "findings": [] })).is_err());
        assert!(parse_findings(&json!({})).is_err());
    }

    #[tokio::test]
    async fn report_findings_acks_what_it_recorded_and_why_it_refused_the_rest() {
        let tool = ReportFindingsTool;
        let ok = tool
            .execute(json!({ "findings": [cited("a"), { "claim": "b" }, { "nope": 1 }] }))
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(ok.output["recorded"], 2);
        assert_eq!(ok.output["rejected"][0]["finding"], 3);
        assert!(ok.output["action_needed"]
            .as_str()
            .unwrap()
            .starts_with("1 of 2 finding(s) have no usable citation"));
        assert!(ok.output["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().starts_with("finding 2:")));

        let none = tool
            .execute(json!({ "findings": [{ "nope": 1 }] }))
            .await
            .unwrap();
        assert!(!none.success);
        assert!(
            !tool
                .execute(json!({ "findings": [] }))
                .await
                .unwrap()
                .success
        );
    }

    fn invocation(name: &str, input: Value) -> ToolInvocation {
        ToolInvocation {
            tool_name: name.to_string(),
            input,
            output: json!(null),
            success: true,
            policy_decision: "allow".to_string(),
            duration_ms: 0,
            denial_reason: None,
            error: None,
        }
    }

    #[test]
    fn collate_aggregates_findings_per_element_and_ignores_other_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            workforce_agent::artifacts::FsArtifactStore::new(tmp.path().to_path_buf()).unwrap();

        let invocations = vec![
            invocation("web.fetch", json!({ "url": "x" })),
            invocation(
                "report_findings",
                json!({ "findings": [cited("a"), { "bad": true }, cited("b")] }),
            ),
            invocation("report_findings", json!({ "findings": [cited("c")] })),
        ];
        let collated = collate_findings(Some(store.as_ref()), "agent", &invocations);
        let claims: Vec<&str> = collated.findings.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, ["a", "b", "c"], "the bad element costs only itself");
        let staged = collated.staged.unwrap();
        assert_eq!(staged["kind"], "tabular_artifact");
        assert_eq!(staged["rows"], 3);
        assert!(staged["artifact_id"].as_str().is_some());

        let unstaged = collate_findings(None, "agent", &invocations);
        assert_eq!(
            unstaged.findings.len(),
            3,
            "the structured set survives without a store"
        );
        assert!(unstaged.staged.is_none());

        let none = collate_findings(
            Some(store.as_ref()),
            "agent",
            &[invocation("web.fetch", json!({}))],
        );
        assert!(none.findings.is_empty() && none.staged.is_none());
    }
}
