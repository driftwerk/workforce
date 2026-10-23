//! Adjudicating a research chain's findings before anyone writes about them
//!.
//!
//! `investigate`'s workers record structured findings — a claim, a 0–100
//! confidence, citations. Left to a synthesiser, low-confidence and uncited
//! findings blend into confident prose and lose their sources on the way. So
//! the chain's steps declare a [`ResearchRole`], and the runner settles each
//! finding's [`Standing`] in code, from the workers' recorded output:
//!
//! - a challenger step gets the findings it must try to prove wrong, by id;
//! - the synthesiser gets only the findings that stood up, and the system —
//!   not the model — appends everything it held back, labelled.
//!
//! Nothing here is inferred from what a step happened to produce: an angle
//! declared adversarial whose challenger recorded nothing has findings that
//! were not confirmed, never findings that survived.

use serde_json::Value;
use workforce_common::text::truncate_with_ellipsis;

use crate::claim_verifier::{deserialize_each, Tier};
use crate::findings::{finding_id, Finding, FINDINGS_PRODUCER};
pub use crate::hub_core::jobs::{Angle, ResearchRole};
use crate::investigate::truncate;

/// Where a finding stands once adjudicated. Only `Supported` reaches the
/// report's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Cited, confident, and — when challenged — upheld by a cited verdict.
    Supported,
    LowConfidence,
    /// Its angle was challenged, but no cited verdict upheld it.
    Unconfirmed,
    /// A cited verdict found it wrong.
    Disputed,
    Uncited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `<researcher step>.<finding id>`, e.g. `w0.f2`.
    pub id: String,
    pub finding: Finding,
    pub standing: Standing,
    /// The cited verdict that contradicted it, when `Disputed`.
    pub rebuttal: Option<Finding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger {
    pub entries: Vec<Entry>,
    /// Angles whose researcher recorded no finding.
    pub silent: Vec<String>,
}

/// The task a research step runs with, and — for a synthesiser — the ledger
/// its report is delivered through.
pub fn prepare(
    role: &ResearchRole,
    task: &str,
    upstream: Option<&Value>,
) -> (String, Option<Ledger>) {
    match role {
        ResearchRole::Challenge { target } => {
            (challenge_brief(task, &recorded(upstream, target)), None)
        }
        ResearchRole::Synthesise {
            angles,
            low_confidence_below,
        } => {
            let ledger = Ledger::adjudicate(angles, *low_confidence_below, upstream);
            (ledger.brief(task), Some(ledger))
        }
    }
}

/// The findings a step recorded, read element by element from its job
/// output (`tool_result.claims`). Errs when it recorded some and none can be
/// read.
fn read_recorded(upstream: Option<&Value>, step: &str) -> eyre::Result<Vec<Finding>> {
    let values = upstream
        .and_then(|u| u.get(step))
        .and_then(|s| s.pointer("/tool_result/claims"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    deserialize_each(values).inspect_err(|e| {
        tracing::warn!(target: "research", step, error = %e, "recorded findings unreadable");
    })
}

fn recorded(upstream: Option<&Value>, step: &str) -> Vec<Finding> {
    read_recorded(upstream, step).unwrap_or_default()
}

/// Bounds on what one rendered finding may take, so a verbose worker cannot
/// crowd out the rest.
const CLAIM_MAX_BYTES: usize = 300;
const EVIDENCE_MAX_BYTES: usize = 300;
const CITATIONS_RENDERED: usize = 4;

/// Worker text on one line, bounded, and inert: it quotes pages and tickets,
/// and the report is posted to a channel, where a quoted `@channel` or
/// `<!here>` would otherwise become a live ping.
fn one_line(text: &str, max: usize) -> String {
    inert(&truncate_with_ellipsis(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        max,
    ))
}

fn inert(text: &str) -> String {
    text.replace('<', "‹")
        .replace('>', "›")
        .replace('@', "@\u{2060}")
}

fn cites(f: &Finding) -> String {
    let shown: Vec<String> = f
        .citations
        .iter()
        .take(CITATIONS_RENDERED)
        .map(|c| inert(c))
        .collect();
    match f.citations.len().saturating_sub(CITATIONS_RENDERED) {
        0 => shown.join(", "),
        more => format!("{} +{more} more", shown.join(", ")),
    }
}

fn confidence(f: &Finding) -> String {
    f.confidence.map_or_else(
        || "confidence not given".to_string(),
        |c| format!("confidence {c}"),
    )
}

/// A finding as a worker reads it: id, claim, confidence, citations, evidence.
fn brief_line(id: &str, f: &Finding) -> String {
    let mut line = format!(
        "- [{id}] {} ({}",
        one_line(&f.claim, CLAIM_MAX_BYTES),
        confidence(f)
    );
    if !f.citations.is_empty() {
        line.push_str(&format!("; cites {}", cites(f)));
    }
    line.push(')');
    if !f.evidence.is_empty() {
        line.push_str(&format!(
            " — evidence: {}",
            one_line(&f.evidence, EVIDENCE_MAX_BYTES)
        ));
    }
    line
}

/// Worker findings quote what workers read — web pages, tickets, logs — so
/// they reach another model fenced as untrusted content, as their staged
/// artifact always was.
fn fenced(lines: &str) -> String {
    workforce_agent::security::external_content::wrap_external_content(lines, FINDINGS_PRODUCER)
        .wrapped_content
}

fn challenge_brief(task: &str, findings: &[Finding]) -> String {
    if findings.is_empty() {
        return format!(
            "{task}\n\nThe researcher recorded no findings, so there is nothing to challenge. \
             Finish at once without recording anything."
        );
    }
    let list = findings
        .iter()
        .enumerate()
        .map(|(i, f)| brief_line(&finding_id(i), f))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{task}\n\nTHE FINDINGS TO CHALLENGE:\n{}", fenced(&list))
}

impl Ledger {
    /// Settle every finding's standing. A finding is judged on its own
    /// record first (uncited, low confidence) and, on an adversarial angle,
    /// on the challenger's cited verdicts: any contradiction disputes it,
    /// and without an upholding verdict it is unconfirmed. A verdict with no
    /// citation is an opinion, and counts for nothing either way.
    pub fn adjudicate(
        angles: &[Angle],
        low_confidence_below: u8,
        upstream: Option<&Value>,
    ) -> Self {
        let mut silent = Vec::new();
        let mut entries = Vec::new();
        for angle in angles {
            let findings = match read_recorded(upstream, &angle.researcher) {
                Ok(findings) if findings.is_empty() => {
                    silent.push(truncate(&angle.label, 80));
                    findings
                }
                Ok(findings) => findings,
                Err(_) => {
                    silent.push(format!(
                        "{} (its findings could not be read)",
                        truncate(&angle.label, 80)
                    ));
                    Vec::new()
                }
            };
            let verdicts: Vec<Finding> = angle
                .challenger
                .as_deref()
                .map(|c| recorded(upstream, c))
                .unwrap_or_default()
                .into_iter()
                .filter(|v| !v.citations.is_empty())
                .collect();
            entries.extend(findings.into_iter().enumerate().map(|(i, finding)| {
                let local = finding_id(i);
                let on_this = |tier: Tier| {
                    verdicts.iter().find(|v| {
                        v.challenge
                            .as_ref()
                            .is_some_and(|c| c.target == local && c.verdict == tier)
                    })
                };
                let rebuttal = on_this(Tier::Contradicted).cloned();
                let upheld = on_this(Tier::Verified).is_some();
                let standing = if finding.citations.is_empty() {
                    Standing::Uncited
                } else if rebuttal.is_some() {
                    Standing::Disputed
                } else if angle.challenger.is_some() && !upheld {
                    Standing::Unconfirmed
                } else if finding.confidence.is_none_or(|c| c < low_confidence_below) {
                    Standing::LowConfidence
                } else {
                    Standing::Supported
                };
                Entry {
                    id: format!("{}.{local}", angle.researcher),
                    finding,
                    standing,
                    rebuttal: rebuttal.filter(|_| standing == Standing::Disputed),
                }
            }));
        }
        let ledger = Self { entries, silent };
        tracing::info!(
            target: "research",
            angles = angles.len(),
            adversarial = angles.iter().any(|a| a.challenger.is_some()),
            supported = ledger.count(Standing::Supported),
            low_confidence = ledger.count(Standing::LowConfidence),
            unconfirmed = ledger.count(Standing::Unconfirmed),
            disputed = ledger.count(Standing::Disputed),
            uncited = ledger.count(Standing::Uncited),
            silent_angles = ledger.silent.len(),
            "findings adjudicated"
        );
        ledger
    }

    pub fn count(&self, standing: Standing) -> usize {
        self.with(standing).count()
    }

    fn with(&self, standing: Standing) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.standing == standing)
    }

    /// The supported findings the synthesiser is given; any beyond
    /// [`SYNTH_LISTED`] are delivered with what was held back instead.
    fn supported_lines(&self) -> String {
        self.with(Standing::Supported)
            .take(SYNTH_LISTED)
            .map(|e| brief_line(&e.id, &e.finding))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn over_limit(&self) -> impl Iterator<Item = &Entry> {
        self.with(Standing::Supported).skip(SYNTH_LISTED)
    }

    /// The synthesiser's task: the supported findings, and only those.
    fn brief(&self, task: &str) -> String {
        let held_back = self.entries.len() - self.count(Standing::Supported).min(SYNTH_LISTED);
        let set_aside = if held_back == 0 {
            String::new()
        } else {
            format!(
                "\n\n{held_back} other finding(s) are not listed here (low confidence, disputed, \
                 unconfirmed, uncited or over the limit). The system appends them to your report, labelled — do \
                 not restate or allude to them."
            )
        };
        if self.count(Standing::Supported) == 0 {
            return format!(
                "{task}\n\nNo finding stood up to adjudication: none was cited, confident and \
                 (where challenged) upheld. Say so plainly in a sentence or two. Do not answer the \
                 question from your own knowledge.{set_aside}"
            );
        }
        format!(
            "{task}\n\nADJUDICATED FINDINGS — the system kept only those that are cited, confident \
             and (where challenged) upheld. Write the report from these alone, and cite every \
             statement with its finding's citations:\n{}{set_aside}",
            fenced(&self.supported_lines())
        )
    }

    /// The report as delivered: the synthesiser's body, or the supported
    /// findings themselves when it produced none, followed by everything
    /// held back. Appended here, in code, so no synthesiser can drop it.
    pub fn deliver(&self, synthesis: Result<String, String>) -> String {
        let body = match synthesis.map(|s| s.trim().to_string()) {
            Ok(body) if !body.is_empty() => body,
            failed => {
                let reason = failed
                    .err()
                    .unwrap_or_else(|| "it wrote nothing".to_string());
                let findings = match self.supported_lines() {
                    lines if lines.is_empty() => "No finding stood up to adjudication.".to_string(),
                    lines => lines,
                };
                format!(
                    "The synthesiser could not write the report ({}), so these are the \
                     findings that stood up, as recorded:\n{findings}",
                    truncate(&reason, 200)
                )
            }
        };
        match self.held_back() {
            Some(held) => format!("{body}\n\n{held}"),
            None => body,
        }
    }

    /// Everything kept out of the body, each group labelled with why.
    fn held_back(&self) -> Option<String> {
        let over_limit =
            format!("Stood up, but beyond the {SYNTH_LISTED} the report was written from:");
        let groups: Vec<(&str, &str, Vec<&Entry>)> = vec![
            (
                "Disputed — a challenger found evidence against these:",
                "disputed",
                self.with(Standing::Disputed).collect(),
            ),
            (
                "Not confirmed — the challenge did not uphold these:",
                "not confirmed",
                self.with(Standing::Unconfirmed).collect(),
            ),
            (
                "Low confidence — leads, not findings:",
                "low confidence",
                self.with(Standing::LowConfidence).collect(),
            ),
            (
                "Uncited — not reported as fact:",
                "uncited",
                self.with(Standing::Uncited).collect(),
            ),
            (&over_limit, "over the limit", self.over_limit().collect()),
        ];
        let groups: Vec<_> = groups
            .into_iter()
            .filter(|(_, _, e)| !e.is_empty())
            .collect();
        let silent = (!self.silent.is_empty()).then(|| {
            (
                format!("No findings recorded for: {}", self.silent.join("; ")),
                format!("{} angle(s) with no findings", self.silent.len()),
            )
        });
        if groups.is_empty() && silent.is_none() {
            return None;
        }
        let counts: Vec<String> = groups
            .iter()
            .map(|(_, name, entries)| format!("{} {name}", entries.len()))
            .chain(silent.as_ref().map(|(_, count)| count.clone()))
            .collect();
        let sections: Vec<String> = groups
            .iter()
            .map(|(label, _, entries)| self.section(label, entries))
            .chain(silent.map(|(line, _)| line))
            .collect();
        Some(format!(
            "{HELD_BACK_HEADING} ({}):\n{}",
            counts.join(", "),
            sections.join("\n")
        ))
    }

    fn section(&self, label: &str, entries: &[&Entry]) -> String {
        let lines: Vec<String> = entries
            .iter()
            .take(HELD_BACK_LISTED)
            .map(|e| self.held_back_line(e))
            .collect();
        let more = match entries.len().saturating_sub(HELD_BACK_LISTED) {
            0 => String::new(),
            n => format!("\n- …and {n} more"),
        };
        format!("{label}\n{}{more}", lines.join("\n"))
    }

    fn held_back_line(&self, e: &Entry) -> String {
        let claim = one_line(&e.finding.claim, CLAIM_MAX_BYTES);
        match (&e.standing, &e.rebuttal) {
            (Standing::Disputed, Some(r)) => format!(
                "- {claim} ({}) — challenged: {} ({})",
                cites(&e.finding),
                one_line(&r.claim, CLAIM_MAX_BYTES),
                cites(r)
            ),
            (Standing::Uncited, _) => format!("- {claim}"),
            (Standing::LowConfidence, _) => {
                format!(
                    "- {claim} ({}; {})",
                    confidence(&e.finding),
                    cites(&e.finding)
                )
            }
            _ => format!("- {claim} ({})", cites(&e.finding)),
        }
    }
}

/// Opens the held-back block, so [`fit`] can find it. The heading line carries
/// every group's count, so the totals survive however much [`fit`] trims.
const HELD_BACK_HEADING: &str = "⚠ Held back from the report";
/// Held-back findings listed per group before the rest are counted.
const HELD_BACK_LISTED: usize = 8;
/// Supported findings the synthesiser is given; more would crowd its context
/// and drown the strongest. The rest are delivered with what was held back.
const SYNTH_LISTED: usize = 60;

/// Fit a delivered report into `max` characters (an ellipsis included). A
/// channel's length limit must never be what silently drops the caveats, so
/// the held-back block takes up to half the room — trimmed item by item, its
/// heading and group labels always kept — and the body is cut to the rest.
pub fn fit(report: &str, max: usize) -> String {
    if report.chars().count() <= max {
        return report.to_string();
    }
    let cut = |text: &str, room: usize| truncate(text, room.saturating_sub(1));
    let Some(at) = report.rfind(HELD_BACK_HEADING) else {
        return cut(report, max);
    };
    let (body, held) = report.split_at(at);
    let held = squeeze(held, max / 2);
    match max.saturating_sub(held.chars().count() + 2) {
        room if room < 2 => cut(&held, max),
        room => format!("{}\n\n{held}", cut(body.trim_end(), room)),
    }
}

/// Item lines of a held-back block, as rendered by [`Ledger::section`].
const ITEM: &str = "- ";
/// Stands in for a group's items trimmed by [`squeeze`].
const TRIMMED: &str = "- …";

/// Trim a held-back block toward `budget` characters by dropping item lines,
/// group by group, marking each trimmed group. The heading and group labels
/// are never dropped: they say what was held back and how much.
fn squeeze(held: &str, budget: usize) -> String {
    let len = |line: &str| line.chars().count() + 1;
    if held.chars().count() <= budget {
        return held.to_string();
    }
    let lines: Vec<&str> = held.lines().collect();
    let labels = lines.iter().filter(|l| !l.starts_with(ITEM)).count();
    let essential: usize = lines
        .iter()
        .filter(|l| !l.starts_with(ITEM))
        .map(|l| len(l))
        .sum::<usize>()
        + labels * len(TRIMMED);
    let mut spare = budget.saturating_sub(essential);
    let mut out: Vec<&str> = Vec::new();
    let mut trimming = false;
    for line in lines {
        if !line.starts_with(ITEM) {
            if trimming {
                out.push(TRIMMED);
            }
            trimming = false;
            out.push(line);
        } else if !trimming && len(line) <= spare {
            spare -= len(line);
            out.push(line);
        } else {
            trimming = true;
        }
    }
    if trimming {
        out.push(TRIMMED);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Challenge;
    use serde_json::json;

    fn finding(claim: &str, confidence: u8, citations: &[&str]) -> Finding {
        Finding {
            claim: claim.to_string(),
            confidence: Some(confidence),
            citations: citations.iter().map(|c| c.to_string()).collect(),
            evidence: String::new(),
            tags: Vec::new(),
            challenge: None,
        }
    }

    fn verdict(target: &str, tier: Tier, citations: &[&str]) -> Finding {
        Finding {
            challenge: Some(Challenge {
                target: target.to_string(),
                verdict: tier,
            }),
            ..finding(&format!("on {target}"), 90, citations)
        }
    }

    fn upstream(steps: &[(&str, Vec<Finding>)]) -> Value {
        Value::Object(
            steps
                .iter()
                .map(|(step, claims)| {
                    (
                        step.to_string(),
                        json!({ "tool_result": { "claims": claims } }),
                    )
                })
                .collect(),
        )
    }

    fn angle(researcher: &str, challenger: Option<&str>) -> Angle {
        Angle {
            label: format!("angle {researcher}"),
            researcher: researcher.to_string(),
            challenger: challenger.map(str::to_string),
        }
    }

    fn standings(ledger: &Ledger) -> Vec<(&str, Standing)> {
        ledger
            .entries
            .iter()
            .map(|e| (e.finding.claim.as_str(), e.standing))
            .collect()
    }

    /// The synthesiser surfaces low-confidence findings and drops uncited
    /// ones from the body — both decided here, not by the model.
    #[test]
    fn low_confidence_and_uncited_findings_never_reach_the_synthesiser() {
        let up = upstream(&[(
            "w0",
            vec![
                finding("solid", 90, &["a.rs:1"]),
                finding("hunch", 30, &["a.rs:2"]),
                finding("hearsay", 95, &[]),
                Finding {
                    confidence: None,
                    ..finding("unrated", 0, &["a.rs:3"])
                },
            ],
        )]);
        let (task, ledger) = prepare(
            &ResearchRole::Synthesise {
                angles: vec![angle("w0", None)],
                low_confidence_below: 60,
            },
            "Synthesise.",
            Some(&up),
        );
        let ledger = ledger.unwrap();
        assert_eq!(
            standings(&ledger),
            [
                ("solid", Standing::Supported),
                ("hunch", Standing::LowConfidence),
                ("hearsay", Standing::Uncited),
                ("unrated", Standing::LowConfidence),
            ]
        );
        assert!(task.contains("[w0.f1] solid (confidence 90; cites a.rs:1)"));
        assert!(
            task.contains("EXTERNAL_UNTRUSTED_CONTENT_"),
            "worker text reaches the synthesiser fenced as untrusted"
        );
        for held in ["hunch", "hearsay", "unrated"] {
            assert!(
                !task.contains(held),
                "{held} must not reach the synthesiser"
            );
        }

        let report = ledger.deliver(Ok("Solid is true (a.rs:1).".to_string()));
        assert!(report.starts_with("Solid is true (a.rs:1)."));
        assert!(report.contains("⚠ Held back from the report (2 low confidence, 1 uncited):"));
        assert!(report
            .contains("Low confidence — leads, not findings:\n- hunch (confidence 30; a.rs:2)"));
        assert!(report.contains("- unrated (confidence not given; a.rs:3)"));
        assert!(report.contains("Uncited — not reported as fact:\n- hearsay"));
    }

    /// Adversarial mode reports only what survives, and marks what a cited
    /// verdict contradicted. An uncited verdict counts for nothing.
    #[test]
    fn an_adversarial_angle_reports_only_what_survives() {
        let up = upstream(&[
            (
                "w0",
                vec![
                    finding("upheld", 90, &["a.rs:1"]),
                    finding("refuted", 90, &["a.rs:2"]),
                    finding("ignored", 90, &["a.rs:3"]),
                    finding("hand-waved", 90, &["a.rs:4"]),
                ],
            ),
            (
                "c0",
                vec![
                    verdict("f1", Tier::Verified, &["a.rs:1"]),
                    verdict("f2", Tier::Verified, &["a.rs:2"]),
                    verdict("f2", Tier::Contradicted, &["b.rs:9"]),
                    verdict("f4", Tier::Verified, &[]),
                    verdict("f9", Tier::Contradicted, &["b.rs:1"]),
                ],
            ),
        ]);
        let ledger = Ledger::adjudicate(&[angle("w0", Some("c0"))], 60, Some(&up));
        assert_eq!(
            standings(&ledger),
            [
                ("upheld", Standing::Supported),
                ("refuted", Standing::Disputed),
                ("ignored", Standing::Unconfirmed),
                ("hand-waved", Standing::Unconfirmed),
            ]
        );
        let report = ledger.deliver(Ok("Body.".to_string()));
        assert!(report.contains(
            "Disputed — a challenger found evidence against these:\n- refuted (a.rs:2) — challenged: on f2 (b.rs:9)"
        ));
        assert!(report.contains("Not confirmed — the challenge did not uphold these:\n- ignored (a.rs:3)\n- hand-waved (a.rs:4)"));
    }

    /// The fail-open case for the new mode: a challenger that crashed and
    /// recorded nothing must not make its researcher's findings read as
    /// having survived.
    #[test]
    fn a_challenger_that_recorded_nothing_confirms_nothing() {
        let up = upstream(&[("w0", vec![finding("claim", 95, &["a.rs:1"])])]);
        let ledger = Ledger::adjudicate(&[angle("w0", Some("c0"))], 60, Some(&up));
        assert_eq!(standings(&ledger), [("claim", Standing::Unconfirmed)]);
        let (task, _) = prepare(
            &ResearchRole::Synthesise {
                angles: vec![angle("w0", Some("c0"))],
                low_confidence_below: 60,
            },
            "S.",
            Some(&up),
        );
        assert!(task.contains("No finding stood up"));
    }

    #[test]
    fn a_silent_researcher_is_named_not_glossed_over() {
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&json!({ "w0": {} })));
        assert!(ledger.entries.is_empty());
        let report = ledger.deliver(Ok("Nothing found.".to_string()));
        assert!(report.contains("No findings recorded for: angle w0"));
    }

    /// A finding with no confidence is the weakest, even under a bar of 0.
    #[test]
    fn no_confidence_is_never_supported() {
        let up = upstream(&[(
            "w0",
            vec![Finding {
                confidence: None,
                ..finding("unrated", 0, &["a.rs:3"])
            }],
        )]);
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 0, Some(&up));
        assert_eq!(standings(&ledger), [("unrated", Standing::LowConfidence)]);
    }

    /// Worker text quoting a broadcast or a mention reaches the channel inert.
    #[test]
    fn quoted_mentions_are_delivered_inert() {
        let up = upstream(&[(
            "w0",
            vec![finding(
                "ping @channel and <!here> <@U123>",
                10,
                &["a.rs:1"],
            )],
        )]);
        let report = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&up))
            .deliver(Ok("Body.".to_string()));
        assert!(
            !report.contains("@channel") && !report.contains("<!here>") && !report.contains("<@U")
        );
        assert!(report.contains("ping @\u{2060}channel and ‹!here› ‹@\u{2060}U123›"));
    }

    /// Findings that exist but cannot be read are not an angle with nothing
    /// to report.
    #[test]
    fn an_unreadable_researcher_is_named_as_such() {
        let up = json!({ "w0": { "tool_result": { "claims": [{ "confidence": 1 }, "junk"] } } });
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&up));
        assert!(ledger.entries.is_empty());
        assert!(ledger
            .deliver(Ok("Body.".to_string()))
            .contains("No findings recorded for: angle w0 (its findings could not be read)"));
    }

    /// Supported findings beyond what the synthesiser is given are delivered
    /// with the held-back block, counted — never silently dropped.
    #[test]
    fn supported_findings_over_the_limit_are_delivered_not_dropped() {
        let findings: Vec<Finding> = (0..SYNTH_LISTED + 2)
            .map(|i| finding(&format!("finding {i}"), 90, &["a.rs:1"]))
            .collect();
        let (task, ledger) = prepare(
            &ResearchRole::Synthesise {
                angles: vec![angle("w0", None)],
                low_confidence_below: 60,
            },
            "S.",
            Some(&upstream(&[("w0", findings)])),
        );
        let last_listed = format!("finding {} ", SYNTH_LISTED - 1);
        assert!(task.contains(&last_listed));
        assert!(!task.contains(&format!("finding {} ", SYNTH_LISTED)));
        assert!(task.contains("2 other finding(s) are not listed here"));
        let report = ledger.unwrap().deliver(Ok("Body.".to_string()));
        assert!(report.contains("(2 over the limit)"));
        assert!(report.contains(&format!(
            "Stood up, but beyond the {SYNTH_LISTED} the report was written from:\n- finding {} (a.rs:1)",
            SYNTH_LISTED
        )));
    }

    /// Recorded output is read element by element: one corrupt entry costs
    /// only itself.
    #[test]
    fn a_corrupt_recorded_finding_costs_only_itself() {
        let up = json!({ "w0": { "tool_result": { "claims": [
            { "claim": "good", "confidence": 90, "citations": ["a.rs:1"] },
            { "confidence": "nope" },
            "junk"
        ] } } });
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&up));
        assert_eq!(standings(&ledger), [("good", Standing::Supported)]);
    }

    #[test]
    fn a_challenger_is_briefed_with_the_ids_it_must_target() {
        let up = upstream(&[(
            "w0",
            vec![finding("x", 80, &["a.rs:1"]), finding("y", 70, &["b.rs:2"])],
        )]);
        let (task, ledger) = prepare(
            &ResearchRole::Challenge {
                target: "w0".to_string(),
            },
            "Prove it wrong.",
            Some(&up),
        );
        assert!(ledger.is_none());
        assert!(task.contains("- [f1] x (confidence 80; cites a.rs:1)"));
        assert!(task.contains("EXTERNAL_UNTRUSTED_CONTENT_"));
        assert!(task.contains("- [f2] y (confidence 70; cites b.rs:2)"));

        let (empty, _) = prepare(
            &ResearchRole::Challenge {
                target: "w0".to_string(),
            },
            "Prove it wrong.",
            None,
        );
        assert!(empty.contains("nothing to challenge"));
    }

    /// A failed synthesiser still delivers the adjudicated findings — never
    /// "could not complete" dressed up as a report.
    #[test]
    fn a_failed_synthesis_delivers_the_findings_that_stood_up() {
        let up = upstream(&[(
            "w0",
            vec![
                finding("solid", 90, &["a.rs:1"]),
                finding("hunch", 10, &["a.rs:2"]),
            ],
        )]);
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&up));
        for failed in [
            Err("Job timed out after 600s".to_string()),
            Ok("  ".to_string()),
        ] {
            let report = ledger.deliver(failed);
            assert!(report.starts_with("The synthesiser could not write the report"));
            assert!(report.contains("- [w0.f1] solid"));
            assert!(report.contains("- hunch (confidence 10; a.rs:2)"));
        }
    }

    #[test]
    fn fit_cuts_the_body_and_keeps_what_was_held_back() {
        let up = upstream(&[(
            "w0",
            vec![
                finding("solid", 90, &["a.rs:1"]),
                finding("hunch", 10, &["a.rs:2"]),
            ],
        )]);
        let ledger = Ledger::adjudicate(&[angle("w0", None)], 60, Some(&up));
        let report = ledger.deliver(Ok("body ".repeat(500)));
        let fitted = fit(&report, 300);
        assert!(fitted.chars().count() <= 300, "{}", fitted.chars().count());
        assert!(fitted.starts_with("body body"));
        assert!(fitted.ends_with("- hunch (confidence 10; a.rs:2)"));

        assert_eq!(fit("short", 300), "short");

        let long = |i: usize| format!("{i} {}", "word ".repeat(60));
        let mut findings: Vec<Finding> = (0..20)
            .map(|i| finding(&long(i), 90, &["a.rs:1", "b.rs:2", "c.rs:3"]))
            .collect();
        findings.extend((0..3).map(|i| finding(&long(i), 90, &[])));
        let up = upstream(&[("w0", findings)]);
        let ledger = Ledger::adjudicate(&[angle("w0", Some("c0"))], 60, Some(&up));
        let report = ledger.deliver(Ok("The answer.".to_string()));
        assert!(report.chars().count() > 3000);
        let fitted = fit(&report, 1200);
        assert!(fitted.chars().count() <= 1200, "{}", fitted.chars().count());
        assert!(fitted.starts_with("The answer."), "the body is kept");
        assert!(fitted.contains("⚠ Held back from the report (20 not confirmed, 3 uncited):"));
        assert!(fitted.contains("Not confirmed — the challenge did not uphold these:"));
        assert!(fitted.contains("Uncited — not reported as fact:"));
        assert!(fitted.contains(TRIMMED));
        assert_eq!(fit(&"x".repeat(10), 4), "xxx…");
    }

    #[test]
    fn a_role_round_trips_through_a_persisted_job_config() {
        let role = ResearchRole::Synthesise {
            angles: vec![angle("w0", Some("c0"))],
            low_confidence_below: 55,
        };
        let back: ResearchRole =
            serde_json::from_value(serde_json::to_value(&role).unwrap()).unwrap();
        assert_eq!(back, role);
    }
}
