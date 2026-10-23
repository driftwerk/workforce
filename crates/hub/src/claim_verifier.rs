//! Claim grounding — fact-check a consequential answer's claims against source.
//!
//! An independent read-only pass extracts the answer's factual claims, verifies
//! each against the real source, and tags it by how grounded it is, so the human
//! knows what to trust. The check is a trait so the engine is unit-testable; the
//! live [`AdvisorClaimChecker`] runs the advisor tool loop with the agent's raw
//! `repo.grep`/`repo.read` tools (the codegraph alone won't surface a bare
//! mutation in a controller — the offline proof relied on raw grep).

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::Deserialize;
use workforce_agent::tools::Tool;

use workforce_agent::runner::ToolInvocation;
use workforce_common::text::truncate_with_ellipsis;

use crate::advisor_loop::{AdvisorLoopOutcome, AdvisorLoopRequest};
use crate::agent_turn::ToolLoop;
pub(crate) use crate::hub_core::claims::RawTier;
pub use crate::hub_core::claims::Tier;
use crate::turn_extraction::{first_bracketed_array, strip_code_fence};

/// One factual claim from the answer, with its verification verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub text: String,
    pub tier: Tier,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundingReport {
    pub claims: Vec<Claim>,
    /// Whether the pass got to finish. False when its research deadline cut it
    /// short: the verdicts it did reach stand, but the answer was not covered
    /// end to end — and a caller reporting "checked, clean" on that would be
    /// claiming more than was done.
    pub complete: bool,
}

impl GroundingReport {
    /// Clean when the pass ran to completion, checked at least one claim and
    /// verified every one. A pass that checked nothing, or was cut short, is
    /// not clean: "nothing was contradicted" is not "everything was verified".
    pub fn is_clean(&self) -> bool {
        self.complete
            && !self.claims.is_empty()
            && self.claims.iter().all(|c| c.tier == Tier::Verified)
    }

    fn in_tiers<'a>(&'a self, tiers: &'a [Tier]) -> impl Iterator<Item = &'a Claim> + 'a {
        self.claims.iter().filter(|c| tiers.contains(&c.tier))
    }

    pub fn tier_count(&self, tier: Tier) -> usize {
        self.in_tiers(&[tier]).count()
    }

    /// Render the claims the source contradicts, each with what was found.
    /// Only a contradiction is worth the reader's attention: an unverified
    /// claim means the check could not reach it, which says nothing about
    /// the answer and is logged instead. `None` when nothing is contradicted,
    /// so an ordinary answer never grows a footer.
    pub fn render(&self) -> Option<String> {
        self.render_tiers("Contradicted by source", &[Tier::Contradicted])
    }

    /// Render the claims `source` contradicts or does not contain — for a
    /// closed evidence set such as this turn's tool outputs, where a figure
    /// the evidence lacks is itself the finding.
    pub fn render_unsupported(&self, source: &str) -> Option<String> {
        self.render_tiers(
            &format!("Not supported by {source}"),
            &[Tier::Contradicted, Tier::Unverified],
        )
    }

    fn render_tiers(&self, header: &str, tiers: &[Tier]) -> Option<String> {
        let lines: String = self
            .in_tiers(tiers)
            .filter(|c| !c.text.is_empty())
            .map(claim_line)
            .collect();
        (!lines.is_empty()).then(|| format!("⚠ {header} — verify before acting:\n{lines}"))
    }
}

/// Per-claim display budgets. Claim text and evidence are free model prose with
/// no length contract, so [`MAX_CLAIMS_PER_PASS`] bounds the COUNT while nothing
/// bounded the SIZE — a verbose pass could render an arbitrarily long footer over
/// the answer it annotates. These are a backstop against that, NOT a trim of
/// typical output: they sit above the length a real claim runs to, so an ordinary
/// bullet passes through untouched and only runaway prose is cut.
///
/// Deliberately generous, because the whole point of the block is telling the
/// reader WHAT is contradicted. Cutting at the sibling
/// `failed_tools::ERROR_MAX_BYTES` (200) would leave a filename and a sentence
/// fragment — the signal deleted by truncation instead of by suppression. There
/// is no detail-on-demand path to recover it from, so the budget stays wide
/// enough to carry the reasoning; bounding the footer as a whole belongs at the
/// delivery site, where all five blocks are assembled.
const CLAIM_TEXT_MAX_BYTES: usize = 240;
const CLAIM_EVIDENCE_MAX_BYTES: usize = 400;

/// One bullet's worth of model prose: newlines collapsed (a bullet is one line)
/// and truncated on a UTF-8 boundary to `max`.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_with_ellipsis(&flat, max)
}

pub(crate) fn claim_line(claim: &Claim) -> String {
    let text = one_line(&claim.text, CLAIM_TEXT_MAX_BYTES);
    if claim.evidence.is_empty() {
        format!("- {text}\n")
    } else {
        format!(
            "- {text}: {}\n",
            one_line(&claim.evidence, CLAIM_EVIDENCE_MAX_BYTES)
        )
    }
}

/// A check's findings plus whether it ran to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedClaims {
    pub claims: Vec<Claim>,
    pub complete: bool,
}

#[async_trait]
pub trait ClaimChecker: Send + Sync {
    /// `budget`, when set, splits the check's time between gathering evidence
    /// and writing up what it found, so a slow pass narrows rather than
    /// returning nothing.
    async fn check(
        &self,
        request: &str,
        answer: &str,
        budget: Option<crate::advisor_loop::LoopBudget>,
    ) -> Result<CheckedClaims>;
}

/// Cap on claims verified per grounding pass. [`verification_system`] inlines
/// the same budget so the model spends its rounds on the highest-risk claims
/// instead of timing out across a long list; the post-parse truncate is the
/// hard guarantee on the report.
const MAX_CLAIMS_PER_PASS: usize = 6;

/// Runs the claim check and assembles the report. The check is injected so the
/// engine is testable without a live model + source tools.
pub struct ClaimGrounder {
    checker: Arc<dyn ClaimChecker>,
}

impl ClaimGrounder {
    pub fn new(checker: Arc<dyn ClaimChecker>) -> Self {
        Self { checker }
    }

    pub async fn ground(&self, request: &str, answer: &str) -> Result<GroundingReport> {
        self.ground_by(request, answer, None).await
    }

    /// As [`ClaimGrounder::ground`], but under a `budget` that holds time back
    /// for the verdict, so it is written rather than cut off.
    pub async fn ground_by(
        &self,
        request: &str,
        answer: &str,
        budget: impl Into<Option<crate::advisor_loop::LoopBudget>> + Send,
    ) -> Result<GroundingReport> {
        let checked = self.checker.check(request, answer, budget.into()).await?;
        let n_extracted = checked.claims.len();
        // Worst first, so the cap never drops a contradiction and keeps a
        // verified claim in its place.
        let mut claims = checked.claims;
        claims.sort_by_key(|c| match c.tier {
            Tier::Contradicted => 0,
            Tier::Unverified => 1,
            Tier::Verified => 2,
        });
        claims.truncate(MAX_CLAIMS_PER_PASS);
        let report = GroundingReport {
            claims,
            complete: checked.complete,
        };
        tracing::info!(
            target: "claim_grounding",
            n_extracted,
            n_claims = report.claims.len(),
            n_contradicted = report.tier_count(Tier::Contradicted),
            n_unverified = report.tier_count(Tier::Unverified),
            complete = report.complete,
            "claim grounding"
        );
        Ok(report)
    }
}

/// System prompt for the verification pass, with the [`MAX_CLAIMS_PER_PASS`]
/// budget inlined so the prompt and the truncation can never drift apart.
fn verification_system() -> String {
    format!(
        "You CHECK an answer's factual claims against evidence. You are not researching the \
    question again — that work is done. Nearly every claim about the code IS checkable; leaving \
    one unverified is a last resort, not a safe default.\n\n\
    WHAT TO CHECK. Only claims about the CODEBASE or SYSTEM that source can settle: what the code \
    does, where something is defined, how a flow works, what a config/value/type/permission is, \
    which paths are guarded. NOT meetings, plans, requirements, business process, runtime state, \
    external systems, commit history or the user's own context — those are not source-verifiable \
    and flagging them is noise. The answering agent has tools you do not: email, tickets, \
    monitoring, databases, chat, external APIs. Never infer what it could or could not have done \
    from your own tool list, and never treat a claim as doubtful because YOUR tools cannot reach \
    its evidence — leave such claims out entirely. If the answer is essentially non-code, return \
    an empty array. Check at most {MAX_CLAIMS_PER_PASS}; when more qualify, keep the \
    highest-risk.\n\n\
    HOW TO CHECK. Spend nothing on what is already settled so you can spend properly on what is \
    not.\n\
    1. If a CURRENT SOURCE block is given, check positive claims and file:line citations against \
       it FIRST. It was assembled by the system and read at HEAD, so it is authoritative — a claim \
       it settles is VERIFIED and needs no tool call. Most claims should land here.\n\
    2. A positive claim it does not cover is not unverified — its silence proves nothing, because \
       it is a selected subset. Read the span the claim names, or search for the symbol.\n\
    3. A UNIVERSAL or NEGATIVE claim (\"all\", \"none\", \"every\", \"no X\") ALWAYS needs an \
       independent search, however complete the given source looks. Nothing that shows you a \
       subset can establish that the rest of the repository holds nothing. Hunt a counterexample: \
       search the whole repository for what the claim rules out, and look where the answer did not \
       look.\n\n\
    Otherwise do not explore: one specific piece of evidence per claim, as few reads as \
    possible.\n\n\
    TAGS. \"verified\" — evidence supports the substance. \"contradicted\" — the substance is \
    genuinely false, or you found a counterexample. \"unverified\" — ONLY when nothing could settle \
    it: outside the source, or too vague to test. Running short of time is not a reason; neither is \
    not having looked.\n\n\
    Judge SUBSTANCE, not citation precision: a file:line off by a few lines, or pointing at the \
    right symbol or area, still VERIFIES — line numbers drift and a small offset is not a \
    contradiction. Tag contradicted only when the substance is wrong. Put the file:line evidence in \
    the evidence field. Return ONLY a JSON array of \
    [{{\"claim\": <string>, \"tier\": \"verified\"|\"contradicted\"|\"unverified\", \"evidence\": <string>}}]. \
    No prose, no markdown fences."
    )
}

/// Tool-calling rounds a verification pass may take before it is forced to
/// answer.
///
/// Was 12. Measured over 313 production passes, the round histogram was bimodal:
/// 22% finished in one or two rounds, and **the 12-round cap was the single
/// largest bucket at 25%** — a quarter of passes never converged, were force-
/// terminated, and shipped whatever the forced final call produced. Those rounds
/// were not buying verification, they were wandering, and the reader paid for
/// every one of them on the reply path. Four rounds keeps the passes that
/// converge and stops financing the ones that do not.
const VERIFICATION_MAX_ROUNDS: u32 = 4;

/// Run the advisor tool loop for a grounding pass and parse its reply with
/// `parse`. `kind` labels logs and errors. Shared by every source-grounded
/// review (claim verification, conclusion review).
///
/// A reply that doesn't parse is a formatting failure, not a research failure —
/// the model researched fine and then wrapped the array in prose, or split it
/// across text blocks. So the recovery re-asks for the *same content* as a bare
/// array, with no tools and one round, instead of paying for the whole tool loop
/// a second time (which is what it used to do, doubling the worst case to 24
/// rounds on the reply's critical path).
///
/// `cancel` is the turn's token: a user who stops the turn stops the review too,
/// rather than leaving it burning rounds for an answer nobody is waiting on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_grounding_loop<T>(
    tool_loop: &dyn ToolLoop,
    tools: &[Arc<dyn Tool>],
    model: Option<&str>,
    system_prompt: &str,
    user_message: String,
    kind: &'static str,
    cancel: Option<tokio_util::sync::CancellationToken>,
    budget: Option<crate::advisor_loop::LoopBudget>,
    parse: impl Fn(&str) -> Result<Vec<T>>,
) -> Result<(Vec<T>, bool)> {
    let research = AdvisorLoopRequest {
        system_prompt: system_prompt.to_string(),
        user_message,
        tools: tools.to_vec(),
        model: model.map(str::to_string),
        budget,
        max_rounds: VERIFICATION_MAX_ROUNDS,
        cancel: cancel.clone(),
        label: kind,
    };
    // `complete` tracks the DEADLINE only, deliberately. The round cap is the
    // loop's design bound, so a review that spends its rounds and then writes up
    // has done the work it was budgeted for — `capped` is not a coverage
    // failure and must not be folded in here. Under a 90s budget the deadline
    // rarely fires, so the cap is the ORDINARY ending; counting it against
    // coverage would make `Clean` nearly unreachable and put healthy and broken
    // passes back in the same bucket, which is exactly the blindness this pass
    // exists to remove. `no_deadline_leaves_the_round_cap_in_charge` guards it.
    let (text, complete) = match tool_loop.run(research, &|_, _, _| {}).await {
        AdvisorLoopOutcome::Success {
            text, truncated, ..
        } => (text, !truncated),
        AdvisorLoopOutcome::Cancelled => return Err(eyre!("{kind} review was cancelled")),
        AdvisorLoopOutcome::ProviderError(e) => return Err(eyre!("{kind} review failed: {e}")),
    };
    // An EMPTY write-up is a MISSING verdict, not a malformed one, and the
    // reformat below cannot rescue it: handed `PREVIOUS REPLY:` and nothing, the
    // model correctly answers `[]` — which parses, and reports "nothing
    // contradicted" for a review that checked nothing. Measured on harbor,
    // that path accounted for 49 of 114 passes in 23h and is why a day of
    // grounding checked 8 claims and found 0 problems while looking healthy.
    // Fail instead: the caller reports the review incomplete, which is true.
    if text.trim().is_empty() {
        tracing::warn!(
            target: "grounding",
            kind,
            complete,
            "review produced no write-up — reporting it as incomplete rather than as a clean pass"
        );
        return Err(eyre!("{kind} review produced no write-up"));
    }
    let parse_err = match parse(&text) {
        Ok(items) => return Ok((items, complete)),
        Err(e) => e,
    };
    tracing::warn!(
        target: "grounding",
        kind,
        preview = %text.chars().take(160).collect::<String>(),
        "review output did not parse — re-asking for the same findings as a bare array"
    );
    // No tools, one round: the findings already exist in `text`, and handing the
    // model its own output back is cheaper and likelier to succeed than making
    // it research the answer again.
    let reformat = AdvisorLoopRequest {
        system_prompt: format!(
            "{system_prompt}\n\nThe previous reply could not be parsed. Return the SAME findings \
             it describes as the bare JSON array specified above — no prose, no fences, no \
             commentary. If it contains no findings, return []."
        ),
        user_message: format!("PREVIOUS REPLY:\n{text}"),
        tools: Vec::new(),
        model: model.map(str::to_string),
        budget: None,
        max_rounds: 1,
        cancel,
        label: kind,
    };
    // The reformat can recover findings the write-up holds; it cannot recover
    // an absent verdict. Told "if it contains no findings, return []", it
    // answers `[]` for a truncated array or an "I ran out of time" — so an
    // empty result here is a write-up nobody could read, not a clean pass.
    match tool_loop.run(reformat, &|_, _, _| {}).await {
        AdvisorLoopOutcome::Success { text, .. } => match parse(&text) {
            Ok(items) if items.is_empty() => {
                tracing::warn!(
                    target: "grounding",
                    kind,
                    "reformat found no findings in an unparseable write-up — reporting it as incomplete"
                );
                Err(eyre!("{kind} review's write-up held no readable verdict"))
            }
            Ok(items) => Ok((items, complete)),
            Err(e) => {
                tracing::warn!(target: "grounding", kind, "reformat did not parse either");
                Err(e)
            }
        },
        AdvisorLoopOutcome::Cancelled => Err(eyre!("{kind} review was cancelled")),
        AdvisorLoopOutcome::ProviderError(_) => Err(parse_err),
    }
}

/// Live checker: runs the advisor tool loop with the agent's raw `repo.grep` /
/// `repo.read` tools so it grounds claims against the actual project source.
pub struct AdvisorClaimChecker {
    tool_loop: Arc<dyn ToolLoop>,
    tools: Vec<Arc<dyn Tool>>,
    model: Option<String>,
    cancel: Option<tokio_util::sync::CancellationToken>,
    /// The source `code_context` assembled and injected for this turn.
    ///
    /// This is not the model's own reading: the SYSTEM selected it and read the
    /// bodies at HEAD, so it is current and authoritative, and a claim it
    /// settles is genuinely verified without a single tool call. It reaches the
    /// verifier anyway — folded into the question — but unlabelled, where it
    /// reads as part of what the user asked rather than as the evidence it is.
    ///
    /// What it cannot do is settle a universal or negative claim: it is a
    /// selected subset of the repo, so "nothing else does X" needs a search
    /// regardless of how complete this looks.
    injected_source: String,
}

impl AdvisorClaimChecker {
    pub fn new(
        tool_loop: Arc<dyn ToolLoop>,
        tools: Vec<Arc<dyn Tool>>,
        model: Option<String>,
    ) -> Self {
        Self {
            tool_loop,
            tools,
            model,
            cancel: None,
            injected_source: String::new(),
        }
    }

    /// Stop the review when the turn it annotates is stopped.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Give the check the source that was assembled and injected for this turn.
    pub fn with_injected_source(mut self, source: String) -> Self {
        self.injected_source = source;
        self
    }
}

#[async_trait]
impl ClaimChecker for AdvisorClaimChecker {
    async fn check(
        &self,
        request: &str,
        answer: &str,
        budget: Option<crate::advisor_loop::LoopBudget>,
    ) -> Result<CheckedClaims> {
        let mut user_message = format!(
            "ORIGINAL QUESTION:\n{request}\n\nANSWER TO VERIFY (against the current project's \
             source):\n{answer}"
        );
        if !self.injected_source.trim().is_empty() {
            user_message.push_str(&format!(
                "\n\nCURRENT SOURCE — assembled by the system for this turn and read at HEAD. \
                 This is authoritative: it is not the answer's own reading, and a claim it \
                 settles is VERIFIED, with no tool call needed. Check the answer's positive \
                 claims and its file:line citations against it first.\n\n\
                 It cannot settle everything. It is a SELECTED SUBSET of the repository, so its \
                 silence proves nothing: a claim it does not cover, and every universal or \
                 negative claim (\"all\", \"none\", \"every\", \"no X\"), still needs you to \
                 search the repository yourself and hunt a counterexample.\n\n{}",
                self.injected_source
            ));
        }
        run_grounding_loop(
            self.tool_loop.as_ref(),
            &self.tools,
            self.model.as_deref(),
            &verification_system(),
            user_message,
            "claims",
            self.cancel.clone(),
            budget,
            parse_claims,
        )
        .await
        .map(|(claims, complete)| CheckedClaims { claims, complete })
    }
}

/// Bounds on the tool-call evidence digest fed to the data-claim verifier.
const EVIDENCE_INPUT_MAX_BYTES: usize = 300;
const EVIDENCE_OUTPUT_MAX_BYTES: usize = 1_200;
const EVIDENCE_TOTAL_MAX_BYTES: usize = 40_000;

/// Render this turn's tool invocations as verification evidence. Most recent
/// entries are kept within the total budget (claims usually rest on the
/// latest queries); elided older calls are announced by count. Inputs stay in
/// marker (recorded) form; staged envelopes are already compact
/// (artifact_id + preview). Failed calls render their error.
pub(crate) fn tool_evidence_digest(invocations: &[ToolInvocation]) -> String {
    let rendered: Vec<String> = invocations
        .iter()
        .enumerate()
        .map(|(i, inv)| render_evidence_entry(i + 1, inv))
        .collect();
    let mut kept = 0usize;
    let mut total = 0usize;
    for entry in rendered.iter().rev() {
        let cost = entry.len() + if kept > 0 { 2 } else { 0 };
        if total + cost > EVIDENCE_TOTAL_MAX_BYTES {
            break;
        }
        total += cost;
        kept += 1;
    }
    let elided = rendered.len() - kept;
    let body = rendered[elided..].join("\n\n");
    if elided > 0 {
        format!("({elided} earlier tool calls elided)\n\n{body}")
    } else {
        body
    }
}

fn render_evidence_entry(i: usize, inv: &ToolInvocation) -> String {
    let status = if inv.success { "OK" } else { "FAILED" };
    let input = truncate_with_ellipsis(&inv.input.to_string(), EVIDENCE_INPUT_MAX_BYTES);
    let result = if inv.success {
        format!(
            "output: {}",
            truncate_with_ellipsis(&inv.output.to_string(), EVIDENCE_OUTPUT_MAX_BYTES)
        )
    } else {
        format!(
            "error: {}",
            truncate_with_ellipsis(
                inv.error.as_deref().unwrap_or("failed (no error detail)"),
                EVIDENCE_OUTPUT_MAX_BYTES
            )
        )
    };
    format!(
        "TOOL CALL {i} — {} — {status} ({}ms)\ninput: {input}\n{result}",
        inv.tool_name, inv.duration_ms
    )
}

const DATA_VERIFICATION_SYSTEM: &str = "You fact-check an answer's DATA claims against \
    the evidence from this turn's tool calls. The user message contains a digest of every \
    tool call this turn (inputs, outputs, staged artifact ids). Extract ONLY claims that \
    attribute specific facts to data sources: numbers, counts, totals, names, ids, dates, \
    rankings, comparisons presented as coming from queried or fetched data. Do NOT extract \
    opinions, recommendations, plans, hypotheticals, general knowledge, or claims about the \
    codebase. If the answer makes no data claims, return an empty array. Prioritise the \
    highest-impact claims (headline numbers, totals, absolutes). Verify each against the \
    evidence: when a claim rests on a staged artifact, recompute the figure with data.query \
    (alias the artifact_id in `inputs`); use tool.retrieve for spilled outputs. NEVER query \
    any upstream system — the ONLY admissible evidence is this turn's tool outputs and \
    staged artifacts. Tag each claim: \"verified\" (the evidence supports it; trivial \
    rounding is fine), \"contradicted\" (the evidence shows a different value, or the \
    supporting tool call FAILED), or \"unverified\" (the evidence does not contain it). \
    Put the evidence (tool call number / artifact_id / recomputed value) in the evidence \
    field. Return ONLY a JSON array of \
    [{\"claim\": <string>, \"tier\": \"verified\"|\"contradicted\"|\"unverified\", \"evidence\": <string>}]. \
    No prose, no markdown fences.";

/// Live data checker: evidence digest inline + read-only probes over staged
/// artifacts. Tools are data.query + tool.retrieve ONLY — no upstream re-query
/// by construction.
pub struct DataClaimChecker {
    tool_loop: Arc<dyn ToolLoop>,
    tools: Vec<Arc<dyn Tool>>,
    model: Option<String>,
    evidence: String,
    cancel: Option<tokio_util::sync::CancellationToken>,
}

impl DataClaimChecker {
    pub fn new(
        tool_loop: Arc<dyn ToolLoop>,
        tools: Vec<Arc<dyn Tool>>,
        model: Option<String>,
        evidence: String,
    ) -> Self {
        Self {
            tool_loop,
            tools,
            model,
            evidence,
            cancel: None,
        }
    }

    /// Stop the review when the turn it annotates is stopped.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

#[async_trait]
impl ClaimChecker for DataClaimChecker {
    async fn check(
        &self,
        request: &str,
        answer: &str,
        budget: Option<crate::advisor_loop::LoopBudget>,
    ) -> Result<CheckedClaims> {
        let user_message = format!(
            "ORIGINAL QUESTION:\n{request}\n\nANSWER TO VERIFY:\n{answer}\n\n\
             EVIDENCE — THIS TURN'S TOOL CALLS:\n{}",
            self.evidence
        );
        run_grounding_loop(
            self.tool_loop.as_ref(),
            &self.tools,
            self.model.as_deref(),
            DATA_VERIFICATION_SYSTEM,
            user_message,
            "data_claims",
            self.cancel.clone(),
            budget,
            parse_claims,
        )
        .await
        .map(|(claims, complete)| CheckedClaims { claims, complete })
    }
}

#[derive(Deserialize)]
struct RawClaim {
    claim: String,
    #[serde(default)]
    tier: RawTier,
    #[serde(default)]
    evidence: String,
}

/// Narrow a model reply to its outermost JSON array (tolerating prose / fences)
/// and deserialise it with [`deserialize_each`].
pub fn parse_json_array<T: serde::de::DeserializeOwned>(text: &str) -> Result<Vec<T>> {
    deserialize_each(json_array(text)?)
}

fn json_array(text: &str) -> Result<Vec<serde_json::Value>> {
    let trimmed = text.trim();
    let json = first_bracketed_array(strip_code_fence(trimmed)).unwrap_or(trimmed);
    serde_json::from_str(json).map_err(|e| eyre!("expected a JSON array: {e}"))
}

/// Deserialise each element on its own, dropping any that don't fit — so one
/// malformed entry never voids the rest. An empty array is a real answer
/// ("nothing to report"); a non-empty one in which NOTHING fits is not, because
/// reading it as empty would turn an unreadable verdict into a clean one.
pub(crate) fn deserialize_each<T: serde::de::DeserializeOwned>(
    values: Vec<serde_json::Value>,
) -> Result<Vec<T>> {
    let total = values.len();
    let items: Vec<T> = values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<T>(value).ok())
        .collect();
    match total - items.len() {
        0 => {}
        dropped if items.is_empty() => {
            return Err(eyre!(
                "none of the {dropped} entries had the expected shape"
            ))
        }
        dropped => {
            tracing::warn!(target: "grounding", dropped, kept = items.len(), "dropped malformed entries")
        }
    }
    Ok(items)
}

/// Per-claim parse: an unknown, garbled or missing tier fails safe to
/// [`Tier::Unverified`], never a silent verified. An entry that names no claim
/// at all is kept as an unverified claim with no text — rendered nowhere, but
/// enough that a verdict missing some of its entries is never clean. When no
/// entry can be read, the verdict is an error.
pub fn parse_claims(text: &str) -> Result<Vec<Claim>> {
    let values = json_array(text)?;
    let total = values.len();
    let claims: Vec<Claim> = values
        .into_iter()
        .map(|value| {
            serde_json::from_value::<RawClaim>(value).map_or_else(
                |_| Claim {
                    text: String::new(),
                    tier: Tier::Unverified,
                    evidence: String::new(),
                },
                |raw| Claim {
                    text: raw.claim,
                    tier: raw.tier.into(),
                    evidence: raw.evidence,
                },
            )
        })
        .collect();
    match claims.iter().filter(|c| c.text.is_empty()).count() {
        0 => {}
        unreadable if unreadable == total => {
            return Err(eyre!("none of the {total} entries named a claim"))
        }
        unreadable => {
            tracing::warn!(target: "grounding", unreadable, total, "verdict entries named no claim; counted as unverified")
        }
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_turn::ProviderLoop;
    use workforce_agent::llm::provider::LlmProvider;

    fn claim(text: &str, tier: Tier) -> Claim {
        Claim {
            text: text.to_string(),
            tier,
            evidence: if tier == Tier::Verified {
                String::new()
            } else {
                "evidence".to_string()
            },
        }
    }

    #[test]
    fn clean_only_when_all_verified() {
        assert!(GroundingReport {
            claims: vec![claim("a", Tier::Verified), claim("b", Tier::Verified)],
            complete: true,
        }
        .is_clean());
        assert!(!GroundingReport {
            claims: vec![claim("a", Tier::Verified), claim("b", Tier::Contradicted)],
            complete: true,
        }
        .is_clean());
        assert!(!GroundingReport {
            claims: vec![claim("a", Tier::Unverified)],
            complete: true,
        }
        .is_clean());
    }

    /// The fail-open regression: "nothing contradicted" is not "everything
    /// verified". An empty verdict, or one cut short, must never earn the
    /// clean bill a caller turns into a persisted `Verified`.
    #[test]
    fn nothing_verified_is_never_clean() {
        assert!(!GroundingReport {
            claims: vec![],
            complete: true,
        }
        .is_clean());
        assert!(!GroundingReport {
            claims: vec![claim("a", Tier::Verified)],
            complete: false,
        }
        .is_clean());
    }

    /// A verdict whose every entry is malformed was attempted and cannot be
    /// read — it is an error, not the empty array it would otherwise become.
    #[test]
    fn parse_claims_with_no_readable_entry_is_an_error_not_an_empty_verdict() {
        assert!(parse_claims("[{\"verdict\":\"verified\"},{\"text\":\"x\"}]").is_err());
        assert!(
            parse_claims("[]").unwrap().is_empty(),
            "a real empty verdict stands"
        );
    }

    #[test]
    fn render_lists_only_what_the_evidence_contradicts() {
        let mixed = GroundingReport {
            claims: vec![
                claim("guard at order.rb:355", Tier::Verified),
                Claim {
                    text: "no bare update anywhere".to_string(),
                    tier: Tier::Contradicted,
                    evidence: "rpa_lock_controller.rb:9".to_string(),
                },
                claim("times unconfirmed", Tier::Unverified),
            ],
            complete: true,
        };

        let out = mixed.render().unwrap();
        assert!(out.starts_with("⚠ Contradicted by source — verify before acting:\n"));
        assert!(out.contains("- no bare update anywhere: rpa_lock_controller.rb:9"));
        assert!(
            !out.contains("guard at order.rb:355"),
            "verified claims stay silent"
        );
        assert!(
            !out.contains("times unconfirmed"),
            "unverified is a log line, not a warning"
        );

        // A clean answer never grows a footer.
        let clean = GroundingReport {
            claims: vec![claim("a", Tier::Verified), claim("b", Tier::Verified)],
            complete: true,
        };
        assert!(clean.render().is_none());

        assert!(GroundingReport {
            claims: vec![],
            complete: true
        }
        .render()
        .is_none());
    }

    /// The verifier writes free prose. A paragraph-length claim rendered whole
    /// buries the answer it annotates, so each bullet is flattened to one line
    /// and bounded — while keeping the prefix, where the `file:line` lives.
    #[test]
    fn claim_lines_are_one_bounded_line_each() {
        let report = GroundingReport {
            claims: vec![Claim {
                text: format!("Step 14 uses bestMatchMapsID {}", "blah ".repeat(80)),
                evidence: format!(
                    "Worker.kt:241-245 — still passes\nfirstGradeMapsID {}",
                    "é".repeat(400)
                ),
                tier: Tier::Contradicted,
            }],
            complete: true,
        };

        let out = report.render().unwrap();
        let bullet = out.lines().find(|l| l.starts_with("- ")).unwrap();
        // Flattened: the embedded newline did not split the bullet in two.
        assert_eq!(out.lines().filter(|l| l.starts_with("- ")).count(), 1);
        // Bounded — both halves capped, each with a 3-byte ellipsis.
        assert!(
            bullet.len()
                <= "- [contradicted] : ".len()
                    + CLAIM_TEXT_MAX_BYTES
                    + CLAIM_EVIDENCE_MAX_BYTES
                    + 6,
            "bullet was {} bytes",
            bullet.len()
        );
        // The identifying prefix of each half survives.
        assert!(bullet.contains("Step 14 uses bestMatchMapsID"));
        assert!(bullet.contains("Worker.kt:241-245 — still passes firstGradeMapsID"));
    }

    /// A short claim is untouched, and the no-evidence branch stays a bare line.
    #[test]
    fn short_claims_and_empty_evidence_render_unchanged() {
        let out = GroundingReport {
            claims: vec![Claim {
                text: "times are wrong".to_string(),
                evidence: String::new(),
                tier: Tier::Contradicted,
            }],
            complete: true,
        }
        .render()
        .unwrap();
        assert!(out.contains("- times are wrong\n"));
        assert!(!out.contains("times are wrong:"));
    }

    #[test]
    fn parse_claims_reads_tiers_and_tolerates_fences() {
        let claims = parse_claims(
            "Here:\n```json\n[{\"claim\":\"x\",\"tier\":\"verified\"},\
             {\"claim\":\"y\",\"tier\":\"contradicted\",\"evidence\":\"f.rb:1\"}]\n```",
        )
        .unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].tier, Tier::Verified);
        assert_eq!(claims[1].tier, Tier::Contradicted);
        assert_eq!(claims[1].evidence, "f.rb:1");
    }

    #[test]
    fn parse_claims_errors_on_non_array() {
        assert!(parse_claims("not json").is_err());
    }

    #[test]
    fn parse_claims_one_bad_element_does_not_void_the_rest() {
        let claims = parse_claims(
            "[{\"claim\":\"a\",\"tier\":\"WEIRD\"},\
             {\"claim\":\"b\",\"tier\":\"contradicted\",\"evidence\":\"f.rb:1\"},\
             {\"tier\":\"verified\"}]",
        )
        .unwrap();
        let tiers: Vec<(&str, Tier)> = claims.iter().map(|c| (c.text.as_str(), c.tier)).collect();
        assert_eq!(
            tiers,
            [
                ("a", Tier::Unverified),
                ("b", Tier::Contradicted),
                ("", Tier::Unverified)
            ]
        );
    }

    /// The partial-drop fail-open: a verdict that loses an entry, or the tier
    /// of one, must not read clean on the entries it kept.
    #[test]
    fn a_verdict_missing_entries_or_tiers_is_never_clean() {
        for verdict in [
            "[{\"claim\":\"a\",\"tier\":\"verified\"},{\"claim\":\"b\"}]",
            "[{\"claim\":\"a\",\"tier\":\"verified\"},{\"tier\":\"contradicted\"}]",
        ] {
            let report = GroundingReport {
                claims: parse_claims(verdict).unwrap(),
                complete: true,
            };
            assert!(!report.is_clean(), "{verdict}");
            assert!(report
                .render_unsupported("x")
                .is_none_or(|r| !r.contains("- \n")));
        }
    }

    /// The per-pass cap keeps the worst claims: a contradiction past the cap
    /// must not fall off and leave a clean report behind.
    #[tokio::test]
    async fn the_claim_cap_never_drops_a_contradiction() {
        let mut claims: Vec<Claim> = (0..MAX_CLAIMS_PER_PASS)
            .map(|i| claim(&format!("ok {i}"), Tier::Verified))
            .collect();
        claims.push(claim("wrong", Tier::Contradicted));
        let report = ClaimGrounder::new(Arc::new(FixedChecker(claims)))
            .ground("q", "a")
            .await
            .unwrap();
        assert_eq!(report.claims.len(), MAX_CLAIMS_PER_PASS);
        assert!(!report.is_clean());
        assert!(report.render().unwrap().contains("wrong"));
    }

    #[test]
    fn an_unverified_only_report_renders_nothing() {
        assert!(GroundingReport {
            claims: vec![claim("could not confirm", Tier::Unverified)],
            complete: true,
        }
        .render()
        .is_none());
    }

    struct FixedChecker(Vec<Claim>);
    #[async_trait]
    impl ClaimChecker for FixedChecker {
        async fn check(
            &self,
            _request: &str,
            _answer: &str,
            _budget: Option<crate::advisor_loop::LoopBudget>,
        ) -> Result<CheckedClaims> {
            Ok(CheckedClaims {
                claims: self.0.clone(),
                complete: true,
            })
        }
    }

    #[test]
    fn verification_prompt_keeps_the_verifier_out_of_the_agents_shoes() {
        let system = verification_system();
        assert!(system.contains("The answering agent has tools you do not"));
        assert!(system.contains("never treat a claim as doubtful because YOUR tools cannot reach"));
    }

    #[test]
    fn verification_prompt_carries_the_claim_budget() {
        assert!(
            verification_system().contains(&format!("Check at most {MAX_CLAIMS_PER_PASS}")),
            "the prompt budget must track MAX_CLAIMS_PER_PASS"
        );
    }

    #[tokio::test]
    async fn ground_caps_claims_at_the_per_pass_limit() {
        let many: Vec<Claim> = (0..MAX_CLAIMS_PER_PASS + 3)
            .map(|i| claim(&format!("claim {i}"), Tier::Verified))
            .collect();
        let report = ClaimGrounder::new(Arc::new(FixedChecker(many)))
            .ground("q", "a")
            .await
            .unwrap();
        assert_eq!(report.claims.len(), MAX_CLAIMS_PER_PASS);
        assert_eq!(
            report.claims[0].text, "claim 0",
            "the head of the list is kept"
        );
        assert_eq!(
            report.claims[MAX_CLAIMS_PER_PASS - 1].text,
            format!("claim {}", MAX_CLAIMS_PER_PASS - 1)
        );

        // At or below the cap, nothing is dropped.
        let exact: Vec<Claim> = (0..MAX_CLAIMS_PER_PASS)
            .map(|i| claim(&format!("claim {i}"), Tier::Unverified))
            .collect();
        let report = ClaimGrounder::new(Arc::new(FixedChecker(exact)))
            .ground("q", "a")
            .await
            .unwrap();
        assert_eq!(report.claims.len(), MAX_CLAIMS_PER_PASS);
    }

    fn invocation(tool: &str, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool.to_string(),
            input: serde_json::json!({"q": "select 1"}),
            output: if success {
                serde_json::json!({"rows": 3})
            } else {
                serde_json::Value::Null
            },
            success,
            policy_decision: "allow".to_string(),
            duration_ms: 7,
            denial_reason: None,
            error: (!success).then(|| "boom".to_string()),
        }
    }

    #[test]
    fn evidence_entry_format_is_stable() {
        let digest = tool_evidence_digest(&[invocation("jira.search", true)]);
        assert_eq!(
            digest,
            "TOOL CALL 1 — jira.search — OK (7ms)\n\
             input: {\"q\":\"select 1\"}\n\
             output: {\"rows\":3}"
        );
    }

    #[test]
    fn evidence_failed_entry_renders_error_not_output() {
        let digest = tool_evidence_digest(&[invocation("data.query", false)]);
        assert!(digest.contains("— FAILED (7ms)"));
        assert!(digest.contains("error: boom"));
        assert!(!digest.contains("output:"));

        let mut no_detail = invocation("data.query", false);
        no_detail.error = None;
        assert!(tool_evidence_digest(&[no_detail]).contains("error: failed (no error detail)"));
    }

    #[test]
    fn evidence_truncates_input_and_output_on_utf8_boundaries() {
        let mut inv = invocation("t", true);
        inv.input = serde_json::json!({"text": "é".repeat(400)});
        inv.output = serde_json::json!({"text": "é".repeat(2_000)});
        let digest = tool_evidence_digest(&[inv]);
        let input_line = digest.lines().find(|l| l.starts_with("input:")).unwrap();
        let output_line = digest.lines().find(|l| l.starts_with("output:")).unwrap();
        assert!(input_line.ends_with('…'));
        assert!(output_line.ends_with('…'));
        // 300 / 1_200 byte caps plus the "input: "/"output: " prefix and the
        // 3-byte ellipsis.
        assert!(input_line.len() <= "input: ".len() + EVIDENCE_INPUT_MAX_BYTES + 3);
        assert!(output_line.len() <= "output: ".len() + EVIDENCE_OUTPUT_MAX_BYTES + 3);
    }

    #[test]
    fn evidence_budget_keeps_most_recent_and_announces_elision() {
        let big = "x".repeat(3_000);
        let invocations: Vec<ToolInvocation> = (0..40)
            .map(|i| {
                let mut inv = invocation(&format!("tool.{i}"), true);
                inv.output = serde_json::json!({ "blob": big });
                inv
            })
            .collect();
        let digest = tool_evidence_digest(&invocations);
        assert!(digest.len() <= EVIDENCE_TOTAL_MAX_BYTES + 64, "bounded");
        assert!(digest.contains("earlier tool calls elided)"));
        assert!(
            digest.contains("TOOL CALL 40 — tool.39"),
            "most recent kept with its original number"
        );
        assert!(!digest.contains("TOOL CALL 1 — tool.0"), "oldest dropped");
        // Kept entries appear in original order.
        let p39 = digest.find("TOOL CALL 39").unwrap();
        let p40 = digest.find("TOOL CALL 40").unwrap();
        assert!(p39 < p40);
    }

    #[test]
    fn evidence_empty_invocations_is_empty() {
        assert_eq!(tool_evidence_digest(&[]), "");
    }

    #[test]
    fn the_data_lens_also_lists_what_its_evidence_lacks() {
        let report = GroundingReport {
            claims: vec![
                claim("wrong", Tier::Contradicted),
                claim("absent", Tier::Unverified),
                claim("fine", Tier::Verified),
            ],
            complete: true,
        };
        let source = report.render().unwrap();
        assert!(source.starts_with("⚠ Contradicted by source — verify before acting:\n"));
        assert!(!source.contains("absent"));
        let data = report.render_unsupported("this turn's data").unwrap();
        assert!(data.starts_with("⚠ Not supported by this turn's data — verify before acting:\n"));
        assert!(data.contains("- wrong: evidence\n- absent: evidence\n"));
        assert!(!data.contains("fine"));
        assert!(GroundingReport {
            claims: vec![claim("fine", Tier::Verified)],
            complete: true,
        }
        .render_unsupported("this turn's data")
        .is_none());
    }

    struct CapturingProvider {
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LlmProvider for CapturingProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[workforce_agent::llm::types::LlmMessage],
            _tools: &[workforce_agent::llm::types::LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<workforce_agent::llm::types::LlmResponse> {
            use workforce_agent::llm::types::{ContentBlock, LlmResponse, StopReason, TokenUsage};
            self.seen
                .lock()
                .unwrap()
                .extend(messages.iter().filter_map(|m| {
                    m.content.iter().find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                }));
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "[{\"claim\":\"42 rows\",\"tier\":\"verified\",\"evidence\":\"tool call 1\"}]"
                        .to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    // ── the reply-path cost bound ──

    /// Answers with a tool call every time, so the loop only ever ends by
    /// hitting its round cap. Records every call and the tools it was offered.
    struct NeverConvergesProvider {
        calls: std::sync::atomic::AtomicUsize,
        tools_per_call: std::sync::Mutex<Vec<usize>>,
        /// Text for the final (tool-less) call, which decides whether the
        /// reformat pass is reached.
        final_text: String,
    }

    impl NeverConvergesProvider {
        fn new(final_text: &str) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                tools_per_call: std::sync::Mutex::new(Vec::new()),
                final_text: final_text.to_string(),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for NeverConvergesProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[workforce_agent::llm::types::LlmMessage],
            tools: &[workforce_agent::llm::types::LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<workforce_agent::llm::types::LlmResponse> {
            use workforce_agent::llm::types::{ContentBlock, LlmResponse, StopReason, TokenUsage};
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.tools_per_call.lock().unwrap().push(tools.len());
            // With tools on the table, keep calling one — never converge.
            let (content, stop_reason) = if tools.is_empty() {
                (
                    vec![ContentBlock::Text {
                        text: self.final_text.clone(),
                    }],
                    StopReason::EndTurn,
                )
            } else {
                (
                    vec![ContentBlock::tool_use(
                        "u1".to_string(),
                        "repo_grep".to_string(),
                        serde_json::json!({}),
                    )],
                    StopReason::ToolUse,
                )
            };
            Ok(LlmResponse {
                content,
                stop_reason,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    fn grep_tool() -> Arc<dyn Tool> {
        struct Noop;
        #[async_trait]
        impl Tool for Noop {
            fn name(&self) -> &str {
                "repo.grep"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
            ) -> Result<workforce_agent::tools::ToolResult> {
                Ok(workforce_agent::tools::ToolResult::success(
                    serde_json::json!({}),
                    0,
                ))
            }
        }
        Arc::new(Noop)
    }

    /// A pass that never converges AND never parses used to cost two full tool
    /// loops — 24 provider calls at the old 12-round cap, every one of them on
    /// the reply's critical path. It now costs one loop plus a single tool-less
    /// reformat call.
    #[tokio::test]
    async fn a_pass_that_never_parses_costs_one_loop_plus_one_reformat() {
        let provider = Arc::new(NeverConvergesProvider::new(
            "sorry, I could not produce JSON",
        ));
        let tools = vec![grep_tool()];
        let result = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            None,
            parse_claims,
        )
        .await;

        assert!(result.is_err(), "unparseable output still fails");
        assert_eq!(
            provider.calls(),
            VERIFICATION_MAX_ROUNDS as usize + 1,
            "one capped research loop plus one reformat — not a second research loop"
        );
        let offered = provider.tools_per_call.lock().unwrap().clone();
        assert_eq!(
            offered.last().copied(),
            Some(0),
            "the reformat call must carry no tools: it re-reads, it does not research"
        );
    }

    /// The prod shape (harbor, 2026-09-18): the research loop ends on its
    /// deadline having written NOTHING, and the reformat then turns that nothing
    /// into `[]` — a clean bill of health for a review that checked nothing.
    /// An empty write-up is a missing verdict, so it must fail, and it must not
    /// spend a reformat call discovering that there is nothing to reformat.
    #[tokio::test]
    async fn an_empty_write_up_fails_instead_of_reformatting_into_no_findings() {
        let provider = Arc::new(NeverConvergesProvider::new(""));
        let tools = vec![grep_tool()];
        let result = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            None,
            parse_claims,
        )
        .await;

        assert!(
            result.is_err(),
            "an empty write-up must not be reported as a review that found nothing"
        );
        assert_eq!(
            provider.calls(),
            VERIFICATION_MAX_ROUNDS as usize,
            "the research loop only — no reformat call, there is nothing to reformat"
        );
    }

    /// Answers the reformat with `[]` and everything else with prose that
    /// holds no array — a write-up cut off before its verdict.
    struct ReformatAnswersEmptyProvider;

    #[async_trait]
    impl LlmProvider for ReformatAnswersEmptyProvider {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            _messages: &[workforce_agent::llm::types::LlmMessage],
            _tools: &[workforce_agent::llm::types::LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<workforce_agent::llm::types::LlmResponse> {
            use workforce_agent::llm::types::{ContentBlock, LlmResponse, StopReason, TokenUsage};
            let text = if system_prompt.is_some_and(|s| s.contains("could not be parsed")) {
                "[]"
            } else {
                "I checked the first two claims and ran out of time before"
            };
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }
    }

    /// The reformat is told "if it contains no findings, return []", so a
    /// write-up that never reached its verdict comes back as `[]` — the same
    /// rubber stamp as an empty write-up, one call later. It must fail.
    #[tokio::test]
    async fn a_reformat_that_finds_no_verdict_fails_instead_of_passing() {
        let result = run_grounding_loop(
            &ProviderLoop(Arc::new(ReformatAnswersEmptyProvider) as Arc<dyn LlmProvider>),
            &[],
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            None,
            parse_claims,
        )
        .await;
        assert!(
            result.is_err(),
            "an unreadable write-up must not become a review that found nothing"
        );
    }

    /// The reformat pass is a real recovery, not just a cheaper failure.
    #[tokio::test]
    async fn a_malformed_final_message_is_recovered_by_the_reformat_pass() {
        let provider = Arc::new(NeverConvergesProvider::new(
            "Here are my findings:\n[{\"claim\":\"x\",\"tier\":\"unverified\",\"evidence\":\"e\"}]",
        ));
        let tools = vec![grep_tool()];
        let claims = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            None,
            parse_claims,
        )
        .await
        .expect("prose-wrapped array is recovered");
        assert_eq!(claims.0.len(), 1);
        assert_eq!(claims.0[0].tier, Tier::Unverified);
    }

    /// The point of the deadline: a pass that would otherwise be killed
    /// mid-research still returns its verdict.
    ///
    /// A wall-clock timeout around the loop can only discard it — every round's
    /// work thrown away, the reader told the check could not run. With the
    /// deadline already passed the loop stops gathering at once, spends its
    /// remaining budget writing up, and reports `complete = false` so the caller
    /// can say the coverage was partial rather than claiming it was clean.
    #[tokio::test]
    async fn an_expired_research_deadline_still_produces_a_verdict() {
        let provider = Arc::new(NeverConvergesProvider::new(
            "[{\"claim\":\"x\",\"tier\":\"unverified\",\"evidence\":\"ran out of time\"}]",
        ));
        let tools = vec![grep_tool()];
        let (claims, complete) = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            // Already elapsed: the first round is the write-up.
            Some(crate::advisor_loop::LoopBudget {
                research_deadline: std::time::Instant::now() - std::time::Duration::from_secs(1),
                write_up_reserve: std::time::Duration::from_secs(30),
            }),
            parse_claims,
        )
        .await
        .expect("an out-of-time pass still reports what it has");

        assert_eq!(claims.len(), 1, "the verdict survives the deadline");
        assert!(
            !complete,
            "and the caller is told the coverage was cut short"
        );
        assert_eq!(
            provider.calls(),
            1,
            "no research rounds once the deadline has passed — straight to the write-up"
        );
        assert_eq!(
            provider.tools_per_call.lock().unwrap().clone(),
            vec![0],
            "the write-up call carries no tools"
        );
    }

    /// Without a deadline the round cap is still the only bound, so every other
    /// caller of the shared loop keeps today's behaviour.
    #[tokio::test]
    async fn no_deadline_leaves_the_round_cap_in_charge() {
        let provider = Arc::new(NeverConvergesProvider::new("[]"));
        let tools = vec![grep_tool()];
        let (_, complete) = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            None,
            None,
            parse_claims,
        )
        .await
        .expect("empty array parses");
        assert_eq!(provider.calls(), VERIFICATION_MAX_ROUNDS as usize);
        assert!(complete, "ending on the round cap is not a truncation");
    }

    /// A stopped turn stops its reviews — they annotate an answer nobody is
    /// waiting for any more.
    #[tokio::test]
    async fn a_cancelled_turn_stops_the_review_before_the_cap() {
        let provider = Arc::new(NeverConvergesProvider::new("[]"));
        let tools = vec![grep_tool()];
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let result = run_grounding_loop(
            &ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>),
            &tools,
            None,
            "system",
            "verify this".to_string(),
            "claims",
            Some(cancel),
            None,
            parse_claims,
        )
        .await;

        assert!(result.is_err(), "a cancelled review yields no report");
        assert!(
            provider.calls() < VERIFICATION_MAX_ROUNDS as usize,
            "cancellation must short-circuit, got {} calls",
            provider.calls()
        );
    }

    // ── the assembled source reaches the checker AS EVIDENCE ──

    /// `code_context` assembles current source and folds it into the message the
    /// model answers. That same string was handed to the verifier as its
    /// `request`, so authoritative HEAD-read source arrived labelled "ORIGINAL
    /// QUESTION" — the largest thing in its prompt and the one thing it most
    /// needed to recognise as evidence. It re-read what it already had, which is
    /// where the rounds and the timeouts went.
    #[tokio::test]
    async fn the_assembled_source_is_presented_as_evidence_not_as_the_question() {
        let provider = Arc::new(CapturingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let checker = AdvisorClaimChecker::new(
            Arc::new(ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>)),
            Vec::new(),
            None,
        )
        .with_injected_source("// crates/hub/src/gate.rs:12\nfn enforce() {}".to_string());
        let _ = checker
            .check(
                "where is it enforced?",
                "It is enforced in gate.rs:12",
                None,
            )
            .await;

        let seen = provider.seen.lock().unwrap();
        let msg = seen.first().expect("user message captured");
        assert!(
            msg.starts_with("ORIGINAL QUESTION:\nwhere is it enforced?"),
            "the question is the question, unpadded: {msg}"
        );
        let evidence_at = msg
            .find("CURRENT SOURCE")
            .expect("source is labelled as evidence");
        assert!(
            evidence_at > msg.find("ANSWER TO VERIFY").unwrap(),
            "and it is presented after the answer, as material to check against"
        );
        assert!(
            msg.contains("authoritative") && msg.contains("no tool call"),
            "told it can settle a claim outright: {msg}"
        );
        assert!(
            msg.contains("universal or negative"),
            "and told what it cannot settle — a subset proves no absence: {msg}"
        );
        assert!(msg.contains("fn enforce()"), "the source itself is carried");
    }

    /// Without an assembled context the checker is unchanged — the question is
    /// still the question and no evidence section is invented.
    #[tokio::test]
    async fn no_assembled_source_means_no_evidence_section() {
        let provider = Arc::new(CapturingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let checker = AdvisorClaimChecker::new(
            Arc::new(ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>)),
            Vec::new(),
            None,
        );
        let _ = checker.check("q", "a", None).await;
        let seen = provider.seen.lock().unwrap();
        let msg = seen.first().expect("user message captured");
        assert!(!msg.contains("CURRENT SOURCE"), "nothing invented: {msg}");
    }

    #[tokio::test]
    async fn data_claim_checker_appends_the_evidence_to_the_user_message() {
        let provider = Arc::new(CapturingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let checker = DataClaimChecker::new(
            Arc::new(ProviderLoop(Arc::clone(&provider) as Arc<dyn LlmProvider>)),
            Vec::new(),
            None,
            tool_evidence_digest(&[invocation("jira.search", true)]),
        );
        let checked = checker.check("how many?", "42 rows", None).await.unwrap();
        assert_eq!(checked.claims.len(), 1);
        assert_eq!(checked.claims[0].tier, Tier::Verified);

        let seen = provider.seen.lock().unwrap();
        let user_message = seen.first().expect("user message captured");
        assert!(user_message.starts_with("ORIGINAL QUESTION:\nhow many?"));
        assert!(user_message.contains("ANSWER TO VERIFY:\n42 rows"));
        assert!(
            user_message.contains("EVIDENCE — THIS TURN'S TOOL CALLS:\nTOOL CALL 1 — jira.search")
        );
    }

    /// The scoping contract the delivery sites re-enter for the data pass:
    /// a `data.query` probe over a session-stamped artifact succeeds under
    /// `CURRENT_AGENT_ID`/`CURRENT_SESSION_ID`/`CURRENT_USER_ID` and fails
    /// closed with no agent scope.
    #[tokio::test]
    async fn data_query_probe_succeeds_under_the_grounding_scopes() {
        use std::sync::LazyLock;
        use tempfile::TempDir;
        use workforce_agent::artifacts::store::{set_global_store, FsArtifactStore};
        use workforce_agent::artifacts::{
            global_store, ArtifactBody, ArtifactDescriptor, ContentType,
        };
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_ID, CURRENT_USER_ID};
        use workforce_agent::tools::data::DataQueryTool;
        use workforce_agent::tools::Tool;

        static DIR: LazyLock<TempDir> = LazyLock::new(|| {
            let dir = TempDir::new().expect("tempdir");
            set_global_store(FsArtifactStore::new(dir.path().to_path_buf()).expect("store"));
            dir
        });
        LazyLock::force(&DIR);
        let store = global_store().expect("store installed");

        let (agent, session, user) = ("dg-agent", "dg-session", "dg-user");
        let rows: Vec<serde_json::Value> = (0..5).map(|i| serde_json::json!({"id": i})).collect();
        let id = CURRENT_USER_ID.sync_scope(user.to_string(), || {
            store
                .write(
                    agent,
                    ArtifactDescriptor {
                        schema: Some(vec!["id".into()]),
                        row_count: Some(rows.len()),
                        session_id: Some(session.to_string()),
                        ..ArtifactDescriptor::from_context(
                            "test",
                            ContentType::ApplicationJson,
                            agent,
                        )
                    },
                    ArtifactBody::Json(serde_json::Value::Array(rows)),
                )
                .unwrap()
                .id
        });

        let probe = serde_json::json!({
            "sql": "SELECT count(*) AS n FROM t",
            "inputs": { "t": id.as_str() },
        });
        let scoped = CURRENT_AGENT_ID
            .scope(
                agent.to_string(),
                CURRENT_SESSION_ID.scope(
                    session.to_string(),
                    CURRENT_USER_ID.scope(user.to_string(), DataQueryTool.execute(probe.clone())),
                ),
            )
            .await
            .unwrap();
        assert!(scoped.success, "{:?}", scoped.error);

        let unscoped = DataQueryTool.execute(probe).await.unwrap();
        assert!(!unscoped.success, "no scope must fail closed");
        assert!(unscoped.error.unwrap().contains("no agent context"));
    }

    #[tokio::test]
    async fn ground_flags_a_contradicted_claim_and_passes_clean_ones() {
        let grounder = ClaimGrounder::new(Arc::new(FixedChecker(vec![
            claim("true thing", Tier::Verified),
            claim("false thing", Tier::Contradicted),
        ])));
        let report = grounder.ground("q", "answer").await.unwrap();
        assert!(!report.is_clean());
        assert!(report.render().unwrap().contains("false thing"));

        let clean = ClaimGrounder::new(Arc::new(FixedChecker(vec![claim("t", Tier::Verified)])));
        assert!(clean.ground("q", "a").await.unwrap().is_clean());

        let empty = ClaimGrounder::new(Arc::new(FixedChecker(vec![])));
        assert!(
            !empty.ground("q", "a").await.unwrap().is_clean(),
            "a pass that checked nothing is not a clean pass"
        );
    }
}
