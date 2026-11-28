//! Goals as the boss keeps them: the result it saves with `crew.plan`, the
//! person's own words checked in their DM before they agree or change one,
//! the first reading and the read-back Desk says once a goal is saved, the
//! facts and checkpoints the boss records on it, the goal's ticket held
//! until it is met, and the work a boss sent with no goal saved for it.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::crew::{self, Party};
use workforce_desk::goal::{
    self, Consent, Evidence, Fact, FactKind, GoalResult, Measure, Met, MetBy, NotReplied, NotSaid,
    Progress, Proposal, Reading, Said,
};
use workforce_desk::plan::Plan;
use workforce_desk::PlanDetail;

use super::github::GithubAccess;
use super::measure;
use super::CrewHub;
use crate::serve::AgentService;

/// The longest one part's reading may take: GitHub slow never holds up a
/// plan turn or the goal checks.
const READ_FOR: std::time::Duration = std::time::Duration::from_secs(30);
/// Why a part read longer than [`READ_FOR`] failed.
const SLOW: &str = "GitHub took over 30 s to answer";

/// How far back the person's words agreeing a goal are looked for.
pub(super) const AGREED_WITHIN: Duration = Duration::hours(24);
/// How far back the person's messages are read, so words older than they
/// may be are said to be so.
const WORDS_LOOKED: Duration = Duration::days(7);
/// The person's messages read for their words, in the DM and in each
/// thread.
const MESSAGES_READ: usize = 200;
/// Failed reads in a row of a part before its address may be repaired.
pub(super) const REPAIR_AFTER: u32 = 3;
/// The most facts one update records.
const FACTS_AT_ONCE: usize = 10;

/// Where Desk looks for the person's words, as a refusal says it.
const WHERE_LOOKED: &str = "the person's own messages to you in your Desk DM and its threads";

/// What a boss is told to do when the person agrees work with it.
pub(super) const SAVE_GOALS: &str = "When the person agrees work with you, save it at once as a \
goal with crew.plan (agreed: their words; result: how it is measured, judged only when nothing \
can measure it), adopting the session already on it, and record what you report as done as facts \
on it (crew.plan_update facts).";

/// Refused when the change applied isn't what the question proposed.
const ANOTHER_PROPOSAL: &str = "Your question about this plan proposed something else, so its \
answer doesn't apply this. Propose this change to the person in your DM, saying the goal and how \
it's measured in the words you'll pass, then pass their reply as person_said: a plain “yes” right \
after it counts. Or ask with crew.ask_person (plan_id and proposal: exactly what you pass here). \
Tell the person only what you propose, never about this check.";
/// Refused when a goal or its result changes without the person's words.
pub(super) const ASK_FIRST: &str = "The goal and how it's measured are the person's. Propose the \
change to them in your DM, saying the goal and how it's measured in the words you'll pass, then \
pass their reply as person_said: a plain “yes” right after it counts. Or ask with crew.ask_person \
(plan_id and proposal: the goal and/or result you'd change it to). If they asked for a new goal \
themselves, pass their words as person_said, with the goal said in their words. Tell the person \
only what you propose, never about this check.";
/// Refused when a plan is given its first goal without the person's words.
const FIRST_GOAL: &str = "A goal is the person's: give this plan its first goal with the words they agreed it in (agreed: {person_said}, from the last 24 hours), with its result.";
/// Why the person's `quote` (`q`) doesn't apply what it answers, when it
/// says no or isn't a clear yes.
fn not_yes(q: &str, says: Consent) -> String {
    match says {
        Consent::No => format!(
            "The person's “{q}” doesn't agree to it: it says no, or asks for something else. Don't apply it: answer what they said, and if you propose something new, put it to them in plain words."
        ),
        _ => format!(
            "Desk couldn't read the person's “{q}” as a clear yes, so it applies nothing: ask them to reply yes or no, in plain words, then pass their reply."
        ),
    }
}

/// Why the person's words `quote` don't count for `said`, and how the boss
/// gets them to.
pub(super) fn not_said(quote: &str, why: NotSaid, said: Said) -> String {
    let q = crew::one_line(quote, 120);
    let ask = " Or ask them (crew.ask_person with plan_id and proposal) and pass their reply, which counts however short.";
    match (why, said) {
        (NotSaid::TooShort { words, chars }, Said::Changing) => format!(
            "“{q}” is too little of what the person wrote to change their goal on its own: quote at least {words} words and {chars} characters of their message, exactly.{ask}"
        ),
        (NotSaid::TooShort { words, chars }, _) => format!(
            "“{q}” is only part of a longer message of the person's: quote their whole message, or at least {words} words and {chars} characters of it, exactly as they wrote it."
        ),
        (NotSaid::NotWhole, _) => format!(
            "“{q}” is only part of the person's reply: their answer is their whole message, which may say otherwise around it. Quote the whole message, exactly as they wrote it, however short."
        ),
        (NotSaid::TooOld(at), Said::Answering) => format!(
            "The person wrote “{q}” before your question ({}): their answer is what they reply after it, on its buttons or in your DM. Wait for their reply, then pass it.",
            at.format("%Y-%m-%d %H:%M UTC")
        ),
        (NotSaid::TooOld(at), Said::Agreeing) => format!(
            "The person wrote “{q}” at {}, more than 24 hours ago: ask them to confirm it in your DM, then quote their reply.",
            at.format("%Y-%m-%d %H:%M UTC")
        ),
        (NotSaid::TooOld(at), Said::Changing) => format!(
            "The person wrote “{q}” at {}, before the goal was last said back to them or more than 24 hours ago, so it doesn't change it now.{ask}",
            at.format("%Y-%m-%d %H:%M UTC")
        ),
        (NotSaid::NotFound, Said::Answering) => format!(
            "Desk looked for “{q}” in {WHERE_LOOKED} since your question, and in their answer on its buttons, and didn't find it word for word. Quote their reply exactly as they wrote it; if they haven't replied yet, wait for it."
        ),
        (NotSaid::NotFound, Said::Agreeing) => format!(
            "Desk looked for “{q}” in {WHERE_LOOKED} over the last 24 hours and didn't find it word for word. Quote one message of theirs exactly as they wrote it: not your own words, and not spread over two messages."
        ),
        (NotSaid::Refuses, _) => not_yes(&q, Consent::No),
        (NotSaid::Unclear, _) => not_yes(&q, Consent::Unclear),
        (NotSaid::NotFound, Said::Changing) => format!(
            "Desk looked for “{q}” in {WHERE_LOOKED} over the last 24 hours and didn't find it word for word. Quote one message of theirs exactly as they wrote it.{ask}"
        ),
    }
}

fn measure_schema() -> Value {
    let repo = json!({ "type": "string", "description": "The GitHub repository as owner/name" });
    let pr = json!({ "type": ["integer", "string"], "description": "The pull request: its number, or its head branch" });
    let runs = json!({ "type": "integer", "minimum": 1, "maximum": goal::MAX_RUNS, "description": "How many of the newest runs, in a row" });
    json!({ "oneOf": [
        { "type": "object", "properties": {
            "kind": { "const": "ci" },
            "repo": repo,
            "workflow": { "type": "string", "description": "The workflow by its name; every workflow when left out" },
            "branch": { "type": "string", "description": "The branch its runs are read on, like main" },
            "want": { "oneOf": [
                { "type": "object", "properties": {
                    "kind": { "const": "minutes" },
                    "per": { "type": "string", "enum": ["run", "job"], "description": "A whole run, or its longest job" },
                    "at_most": { "type": "number", "exclusiveMinimum": 0 },
                    "runs": runs
                }, "required": ["kind", "per", "at_most", "runs"] },
                { "type": "object", "properties": {
                    "kind": { "const": "green" },
                    "runs": runs
                }, "required": ["kind", "runs"] }
            ] }
        }, "required": ["kind", "repo", "branch", "want"], "description": "GitHub Actions runs on a branch: their minutes, or green" },
        { "type": "object", "properties": {
            "kind": { "const": "pr_merged" }, "repo": repo, "pr": pr
        }, "required": ["kind", "repo", "pr"], "description": "A pull request merged" },
        { "type": "object", "properties": {
            "kind": { "const": "checks_green" }, "repo": repo, "pr": pr
        }, "required": ["kind", "repo", "pr"], "description": "A pull request's checks green on its head" },
        { "type": "object", "properties": {
            "kind": { "const": "judged" },
            "evidence": { "type": "string", "description": "What you judge it by, in words: only when nothing above can measure it" }
        }, "required": ["kind", "evidence"], "description": "What nothing can measure: you show checkpoints toward it" }
    ] })
}

/// How a result is said in a tool's input.
pub(super) fn result_schema() -> Value {
    json!({ "type": "object", "properties": {
        "words": { "type": "string", "description": "Done when …, in the person's terms" },
        "parts": { "type": "array", "minItems": 1, "maxItems": goal::MAX_PARTS, "items": measure_schema() }
    }, "required": ["words", "parts"],
    "description": "How done is measured. Desk checks it and closes the goal only when it is met or the person says so" })
}

/// The person's own words, quoted, in a tool's input.
pub(super) fn said_schema(what: &str) -> Value {
    json!({ "type": "object", "properties": {
        "person_said": { "type": "string", "description": format!("The person's own words {what}, quoted exactly as they wrote them in your DM") }
    }, "required": ["person_said"] })
}

/// What a boss's question about a goal proposes, in `crew.ask_person`.
pub(super) fn proposal_schema() -> Value {
    json!({ "type": "object", "properties": {
        "goal": { "type": "string", "description": "The goal you propose it becomes" },
        "result": result_schema(),
        "met": { "type": "string", "description": "That it is met, with what shows it" }
    }, "description": "With plan_id: the change to the goal you ask the person about. Their answer applies only this change" })
}

pub(super) fn facts_schema() -> Value {
    json!({ "type": "array", "maxItems": FACTS_AT_ONCE, "items": { "type": "object", "properties": {
        "said": { "type": "string", "description": "What you report as done toward the goal, in a sentence" },
        "evidence": evidence_schema(),
        "replaces": { "type": "string", "description": "The id of a fact of yours it corrects" }
    }, "required": ["said"] },
    "description": "What you report as done toward the goal, kept with it so no fresh thread loses it. Evidence Desk can find is checked" })
}

pub(super) fn checkpoint_schema() -> Value {
    json!({ "type": "object", "properties": {
        "part": { "type": "integer", "minimum": 1, "description": "The judged part, from 1" },
        "said": { "type": "string" },
        "evidence": evidence_schema()
    }, "required": ["part", "said", "evidence"],
    "description": "What you show toward a judged part of the result" })
}

pub(super) fn repair_schema() -> Value {
    json!({ "type": "object", "properties": {
        "part": { "type": "integer", "minimum": 1, "description": "The part, from 1" },
        "measure": measure_schema()
    }, "required": ["part", "measure"],
    "description": "Where a part is read from, once its newest reads failed: the same kind and bound, read from elsewhere" })
}

fn evidence_schema() -> Value {
    json!({ "type": "array", "items": { "oneOf": [
        { "type": "object", "properties": { "kind": { "const": "run" }, "repo": { "type": "string" }, "id": { "type": "integer" } }, "required": ["kind", "repo", "id"] },
        { "type": "object", "properties": { "kind": { "const": "pull" }, "repo": { "type": "string" }, "number": { "type": "integer" } }, "required": ["kind", "repo", "number"] },
        { "type": "object", "properties": { "kind": { "const": "commit" }, "repo": { "type": "string" }, "sha": { "type": "string" } }, "required": ["kind", "repo", "sha"] },
        { "type": "object", "properties": { "kind": { "const": "ticket" }, "key": { "type": "string" } }, "required": ["kind", "key"] }
    ] } })
}

/// `r` met as `progress` reads it at `at`, in its own words.
pub(super) fn met_by_reading(r: &GoalResult, progress: &Progress, at: DateTime<Utc>) -> Met {
    Met {
        at,
        by: MetBy::Result,
        why: format!("it reads met: {}", goal::progress_line(r, progress)),
    }
}

/// A result as a tool's input says it, checked.
pub(super) fn result_of(v: &Value) -> Result<GoalResult, String> {
    let r: GoalResult =
        serde_json::from_value(v.clone()).map_err(|e| format!("The result doesn't read: {e}."))?;
    goal::validate(&r).map_err(|problems| {
        problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    Ok(r)
}

/// The person's words a tool's input quotes under `field`, when it does.
pub(super) fn said_of(input: &Value, field: &str) -> Result<Option<String>, String> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .get("person_said")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .map(|w| Some(w.to_string()))
            .ok_or_else(|| format!("{field} quotes the person's own words (person_said).")),
    }
}

/// What a question's `proposal` says, checked.
pub(super) fn proposal_of(v: &Value) -> Result<Proposal, String> {
    let p = Proposal {
        goal: v["goal"]
            .as_str()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_string),
        result: match v.get("result").filter(|r| !r.is_null()) {
            Some(r) => Some(result_of(r)?),
            None => None,
        },
        met: v["met"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string),
    };
    match p.is_empty() {
        true => Err("Say what the question proposes: a goal, a result, or met.".into()),
        false => Ok(p),
    }
}

/// What a tool's input says as evidence.
fn evidence_of(v: &Value) -> Result<Vec<Evidence>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        v => serde_json::from_value(v.clone())
            .map_err(|e| format!("The evidence doesn't read: {e}.")),
    }
}

/// The GitHub reads of `service`, as the person in scope.
pub(super) fn access(service: &AgentService) -> GithubAccess<'_> {
    GithubAccess {
        vault: service.vault.clone(),
        global: service.global_vault.clone(),
        config: &service.config,
        registry: &service.tools,
    }
}

/// A fact or checkpoint as the boss records it.
pub(super) struct Recorded {
    pub kind: FactKind,
    pub part: Option<u16>,
    pub said: String,
    pub evidence: Vec<Evidence>,
    pub replaces: Option<Uuid>,
    /// A checkpoint whose evidence was found to be the goal's own new work.
    pub shown: bool,
}

/// The facts and checkpoint a `crew.plan_update` input records, checked
/// against `r`: a checkpoint is toward a judged part, with evidence.
pub(super) fn recorded_of(input: &Value, r: Option<&GoalResult>) -> Result<Vec<Recorded>, String> {
    let mut out = Vec::new();
    let facts = input.get("facts").filter(|f| !f.is_null());
    let checkpoint = input.get("checkpoint").filter(|c| !c.is_null());
    if (facts.is_some() || checkpoint.is_some()) && r.is_none() {
        return Err("Facts and checkpoints are kept on a goal: this plan has no result.".into());
    }
    let said = |v: &Value| {
        v["said"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| crew::clip(s, goal::FACT_MAX))
            .ok_or_else(|| "Say each fact (said).".to_string())
    };
    if let Some(facts) = facts {
        let list = facts.as_array().ok_or("facts is a list.")?;
        if list.len() > FACTS_AT_ONCE {
            return Err(format!("At most {FACTS_AT_ONCE} facts at once."));
        }
        for f in list {
            out.push(Recorded {
                kind: FactKind::Fact,
                part: None,
                said: said(f)?,
                evidence: evidence_of(&f["evidence"])?,
                replaces: match f["replaces"].as_str() {
                    Some(id) => Some(
                        id.trim()
                            .parse()
                            .map_err(|_| "replaces names a fact by its id.".to_string())?,
                    ),
                    None => None,
                },
                shown: false,
            });
        }
    }
    if let (Some(c), Some(r)) = (checkpoint, r) {
        let n = c["part"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=r.parts.len()).contains(n))
            .ok_or("Say which part it is toward (part, from 1).")?;
        if r.parts[n - 1].measured() {
            return Err(format!(
                "Part {n} is measured: Desk reads it itself. Checkpoints are for a judged part."
            ));
        }
        let evidence = evidence_of(&c["evidence"])?;
        if evidence.is_empty() {
            return Err("A checkpoint shows its evidence: a run, a pull request, a commit.".into());
        }
        out.push(Recorded {
            kind: FactKind::Checkpoint,
            part: Some(u16::try_from(n - 1).unwrap_or_default()),
            said: said(c)?,
            evidence,
            replaces: None,
            shown: false,
        });
    }
    Ok(out)
}

/// A repair a `crew.plan_update` input asks of `r`, as the result it
/// makes: allowed once the part's newest [`REPAIR_AFTER`] reads failed,
/// and only where it reads from.
pub(super) fn repaired(
    input: &Value,
    r: &GoalResult,
    progress: &goal::Progress,
) -> Result<Option<GoalResult>, String> {
    let Some(repair) = input.get("repair").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let n = repair["part"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (1..=r.parts.len()).contains(n))
        .ok_or("Say which part to repair (part, from 1).")?;
    let failing = progress.parts.get(n - 1).map_or(0, |p| p.failing);
    if failing < REPAIR_AFTER {
        return Err(format!(
            "Part {n} reads: repair it only once its newest {REPAIR_AFTER} reads failed."
        ));
    }
    let measure: Measure = serde_json::from_value(repair["measure"].clone())
        .map_err(|e| format!("The measure doesn't read: {e}."))?;
    if !r.parts[n - 1].same_but_address(&measure) {
        return Err(
            "A repair changes only where a part is read from (repository, workflow, branch, pull request), never its kind or bound: ask the person to change how it's measured."
                .into(),
        );
    }
    let mut next = r.clone();
    next.parts[n - 1] = measure;
    goal::validate(&next).map_err(|p| {
        p.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    Ok(Some(next))
}

/// What a `crew.plan_update` input does to a plan's goal, checked before
/// anything of the plan changes.
#[derive(Default)]
pub(super) struct GoalEdit {
    /// The result the plan is to have: changed on the person's word, or
    /// repaired.
    pub result: Option<GoalResult>,
    /// The first readings of that result.
    readings: Vec<Reading>,
    /// Whether its goal or result changes, so the person is told it again.
    changes: bool,
    /// The person's answer it applies: their words, the question they
    /// answered (none when they asked for it themselves) and when they
    /// wrote it.
    answered: Option<(String, Option<Uuid>, DateTime<Utc>)>,
    recorded: Vec<Recorded>,
    /// The person's words saying it is met, and the question they
    /// answered, if they answered one.
    met: Option<(String, Option<Uuid>)>,
}

impl GoalEdit {
    /// Whether it does anything to the goal itself.
    pub fn any(&self) -> bool {
        self.answered.is_some() || !self.recorded.is_empty() || self.met.is_some()
    }

    /// The person's answer it applies: their words, the question they
    /// answered and when they wrote it.
    pub fn answer(&self) -> Option<(&str, Option<Uuid>, DateTime<Utc>)> {
        self.answered
            .as_ref()
            .map(|(words, item, at)| (words.as_str(), *item, *at))
    }

    /// What the boss is told when a change to `now`, a goal the person
    /// agreed, waits on them and no answer of theirs came with it.
    pub fn asking(&self, now: &PlanDetail) -> Option<&'static str> {
        let p = &now.summary.plan;
        (self.answered.is_none() && p.covered() && p.approved_revision.is_some() && p.awaiting())
            .then_some(ASK_TO_APPLY)
    }
}

/// Refused when the answer given was applied already.
pub(super) const APPLIED: &str = "That answer of the person's was applied already: ask them about this change (crew.ask_person with plan_id and proposal) and pass their new reply.";

/// Said when a change to an agreed goal waits on the person.
const ASK_TO_APPLY: &str = "Put it to the person (crew.ask_person with plan_id, and proposal when it changes the goal or how it's measured), then pass their words (person_said) to apply it.";

impl CrewHub {
    /// What `input` does to the goal of `detail`, plan of the boss `b`'s:
    /// the person's words it quotes checked against the question they
    /// answer, and a result it changes read once.
    pub(super) async fn goal_edit(
        &self,
        b: &super::tools::Bound,
        detail: &PlanDetail,
        input: &Value,
    ) -> Result<GoalEdit, String> {
        let p = &detail.summary.plan;
        let goal_to = input["goal"]
            .as_str()
            .map(str::trim)
            .filter(|g| !g.is_empty() && *g != p.goal.trim());
        let result_to = match input.get("result").filter(|r| !r.is_null()) {
            Some(r) => Some(result_of(r)?).filter(|r| p.result.as_ref() != Some(r)),
            None => None,
        };
        let repaired = match (&p.result, &detail.goal) {
            (Some(r), Some(progress)) => repaired(input, r, progress)?,
            _ if input.get("repair").is_some_and(|r| !r.is_null()) => {
                return Err("Only a goal's result is repaired: this plan has none.".into())
            }
            _ => None,
        };
        if repaired.is_some() && result_to.is_some() {
            return Err("Repair a part or change the result, not both.".into());
        }
        let mut recorded = recorded_of(input, result_to.as_ref().or(p.result.as_ref()))?;
        if let Some(r) = result_to.as_ref().or(p.result.as_ref()) {
            for c in recorded
                .iter_mut()
                .filter(|c| c.kind == FactKind::Checkpoint)
            {
                let part = c.part.unwrap_or_default();
                c.evidence = self
                    .checkpoint_shown(&b.service, detail, r, part, &c.evidence)
                    .await?;
                c.shown = true;
            }
        }
        let quoted = input["person_said"]
            .as_str()
            .map(str::trim)
            .filter(|w| !w.is_empty());
        let changes = goal_to.is_some() || result_to.is_some();
        // On a goal the person agreed, or one running, its goal and result
        // are theirs to change.
        let theirs = (p.result.is_some() || result_to.is_some())
            && (p.agreed.is_some() || p.approved_revision.is_some());
        // A plan's first goal is agreed as a new one's is: on the person's
        // words of the last day.
        let agreed = said_of(input, "agreed")?;
        if agreed.is_some() && (p.result.is_some() || result_to.is_none()) {
            return Err(match p.result.is_some() {
                true => "agreed gives a plan its first goal; this one has its goal already. To change it, pass the person's words as person_said.".into(),
                false => "agreed gives this plan its first goal: say how it's measured too (result).".into(),
            });
        }
        let answered = match (quoted, agreed) {
            (_, Some(words)) => {
                let at = self.agreed_at(&b.service, &words, goal_to.unwrap_or(&p.goal))?;
                Some((words, None, at))
            }
            (None, None) if changes && theirs && p.result.is_none() => {
                return Err(FIRST_GOAL.into())
            }
            (None, None) if changes && theirs => return Err(ASK_FIRST.into()),
            (None, None) => None,
            (Some(words), None) => {
                let applying = Proposal {
                    goal: goal_to.map(str::to_string),
                    result: result_to.clone(),
                    met: None,
                };
                let (item, at) = self.answer_of(&b.service, p, words, &applying).await?;
                Some((words.to_string(), item, at))
            }
        };
        let met = match said_of(input, "met")? {
            None => None,
            Some(_) if !p.goal_open() => return Err("This plan has no goal left to meet.".into()),
            Some(words) => {
                let applying = Proposal {
                    met: Some(words.clone()),
                    ..Proposal::default()
                };
                let (item, _) = self.answer_of(&b.service, p, &words, &applying).await?;
                Some((words, item))
            }
        };
        let result = result_to.or(repaired);
        let readings = match &result {
            Some(r) => {
                self.first_readings(&b.service, r, p.agreed_at.unwrap_or_else(crew::now))
                    .await?
            }
            None => Vec::new(),
        };
        Ok(GoalEdit {
            result,
            readings,
            changes,
            answered,
            recorded,
            met,
        })
    }

    /// Keeps what `edit` records on plan `now`, the boss `b`'s, once its
    /// change is made: the readings of a result it changed, the goal said
    /// back to the person when it changed, the facts and checkpoint, and
    /// the goal met on their word. What the boss is told of it.
    pub(super) async fn goal_kept(
        &self,
        b: &super::tools::Bound,
        now: &PlanDetail,
        edit: GoalEdit,
    ) -> Result<Value, String> {
        let p = &now.summary.plan;
        if let Some(r) = p.result.as_ref().filter(|_| !edit.readings.is_empty()) {
            self.keep_readings(p.id, r, &edit.readings).await;
        }
        let applied = !p.awaiting() || p.approved_revision.is_none();
        if edit.changes && applied && p.agreed.is_some() {
            let fresh = self.desk.plan(p.id).await.map_err(|e| e.to_string())?;
            self.read_back(&b.service, &fresh).await;
        }
        let by = Party::manager(b.service.agent_id.as_str(), &super::name_of(&b.service));
        let kept = self
            .keep_recorded(&b.service, p.id, edit.recorded, &by)
            .await?;
        let mut out = json!({});
        if !kept.is_empty() {
            out["facts"] = json!(kept
                .iter()
                .map(|(id, checked)| json!({ "id": id, "checked": checked }))
                .collect::<Vec<_>>());
        }
        if let Some((words, item)) = edit.met {
            if let Some(item) = item {
                if !self.ask_applied(item).await? {
                    return Err(APPLIED.into());
                }
            }
            let met = Met {
                at: crew::now(),
                by: MetBy::Person,
                why: format!("the person said: “{}”", crew::one_line(&words, 200)),
            };
            if let Err(e) = self.goal_met(p.id, met).await {
                if let Some(item) = item {
                    self.ask_unapplied(item).await;
                }
                return Err(e);
            }
            out["met"] = json!("The goal is met on the person's word: Desk moved its ticket to Done, and the plan's closing turn comes next, where you write the person its closing summary.");
        }
        Ok(out)
    }

    /// The person's messages to `service` since `since`, in its DM and the
    /// DM's threads: of an answer they gave on an item's buttons, only
    /// what they answered.
    fn person_words(
        &self,
        service: &AgentService,
        since: DateTime<Utc>,
    ) -> Vec<(DateTime<Utc>, String)> {
        let Some(person) = self.person(Some(service)) else {
            return Vec::new();
        };
        let mut out = super::orchestrator::person_messages(service, &person, since, MESSAGES_READ);
        out.extend(super::orchestrator::person_thread_messages(
            service,
            &person,
            since,
            MESSAGES_READ,
        ));
        out.into_iter()
            .map(|(at, text)| (at, goal::answered_text(&text).to_string()))
            .collect()
    }

    /// When the person agreed in `quote`, word for word in one of their
    /// messages of the last day.
    ///
    /// A short message ("ok", "yes go ahead") agrees only to what the boss
    /// said last: one written before the boss's newest DM message answers
    /// something earlier, and counts for nothing now.
    pub(super) fn agreed_at(
        &self,
        service: &AgentService,
        quote: &str,
        goal: &str,
    ) -> Result<DateTime<Utc>, String> {
        let now = crew::now();
        let since = now - AGREED_WITHIN;
        let last = self.person(Some(service)).and_then(|person| {
            super::orchestrator::boss_said(service, &person, MESSAGES_READ)
                .into_iter()
                .map(|(at, _)| at)
                .max()
        });
        let (now_words, earlier): (Vec<_>, Vec<_>) = self
            .person_words(service, now - WORDS_LOOKED)
            .into_iter()
            .partition(|(at, text)| goal::stands_alone(text) || last.is_none_or(|l| *at > l));
        goal::agreed_by(&now_words, quote, since, goal).map_err(|e| {
            match goal::agreed_by(&earlier, quote, since, goal) {
                Ok(_) => format!(
                    "The person's “{}” came before your last message in your DM, so it answered something earlier, not this. Ask them in your DM whether they agree to the goal, then quote their reply.",
                    crew::one_line(quote, 120)
                ),
                Err(_) => not_said(quote, e, Said::Agreeing),
            }
        })
    }

    /// The person's word for `applying`, a change to plan `p`'s goal, in
    /// `quote`: their reply to the question that proposed it, newer than it,
    /// on its buttons or in their messages, however short; or, when no
    /// question of the boss's proposed it, their own words asking for it
    /// unprompted (a goal in their words, or that it is met), newer than the
    /// goal was last said back. The question it answers, if one, and when.
    pub(super) async fn answer_of(
        &self,
        service: &AgentService,
        p: &Plan,
        quote: &str,
        applying: &Proposal,
    ) -> Result<(Option<Uuid>, DateTime<Utc>), String> {
        let asks = self
            .desk
            .store()
            .goal_asks(p.id)
            .await
            .map_err(|e| format!("The questions about this goal couldn't be read: {e}"))?;
        if let Some(ask) = asks
            .iter()
            .find(|a| goal::same_proposal(&a.proposal, applying))
        {
            let mut messages = self.person_words(service, ask.at - WORDS_LOOKED);
            messages.extend(ask.answered.clone());
            let at = goal::said_by(&messages, quote, ask.at, Said::Answering)
                .map_err(|e| not_said(quote, e, Said::Answering))?;
            return Ok((Some(ask.item), at));
        }
        if applying.met.is_none() {
            if let Some(at) = self.replied_at(service, p, quote, applying)? {
                return Ok((None, at));
            }
        }
        // A result is changed only on an answer: its parts are the boss's
        // to put to the person.
        if applying.result.is_some() {
            return Err(match asks.is_empty() {
                true => ASK_FIRST.into(),
                false => ANOTHER_PROPOSAL.into(),
            });
        }
        let now = crew::now();
        let since = [Some(now - AGREED_WITHIN), p.read_back_at, p.agreed_at]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(now - AGREED_WITHIN);
        let messages = self.person_words(service, now - WORDS_LOOKED);
        let at = goal::said_by(&messages, quote, since, Said::Changing).map_err(|e| {
            let lead = match applying.met.is_some() {
                true => "No question of yours proposed that it is met, so the person's own words must say so. ",
                false => "No question of yours proposed this goal, so the person's own words must ask for it. ",
            };
            format!("{lead}{}", not_said(quote, e, Said::Changing))
        })?;
        if let Some(to) = &applying.goal {
            let theirs = messages
                .iter()
                .filter(|(when, _)| *when == at)
                .any(|(_, text)| goal::says_goal(text, to));
            if !theirs {
                return Err(format!(
                    "The person's message with “{}” doesn't say the new goal “{}” in their words: set goal to what they wrote, word for word, or ask them (crew.ask_person with plan_id and proposal: {{goal}}) and pass their reply.",
                    crew::one_line(quote, 120),
                    crew::one_line(to, 120)
                ));
            }
        }
        Ok((None, at))
    }

    /// When `quote` is the person's plain reply ("yes", "ok") to the
    /// boss's own last message before it, in their DM or a thread of it,
    /// which proposed `applying`'s goal and result in the words it passes,
    /// a change to plan `p`'s goal: typed by them since its goal was last
    /// agreed or said back, so one reply applies once. None when it is no
    /// such reply; refused when the message it answers asked more, or
    /// proposed other words.
    fn replied_at(
        &self,
        service: &AgentService,
        p: &Plan,
        quote: &str,
        applying: &Proposal,
    ) -> Result<Option<DateTime<Utc>>, String> {
        let Some(person) = self.person(Some(service)) else {
            return Ok(None);
        };
        let proposed: Vec<(&str, &str)> = [
            applying.goal.as_deref().map(|g| ("the goal", g)),
            applying
                .result
                .as_ref()
                .map(|r| ("how it's measured (the result's words)", r.words.as_str())),
        ]
        .into_iter()
        .flatten()
        .collect();
        let words: Vec<&str> = proposed.iter().map(|(_, w)| *w).collect();
        let now = crew::now();
        let since = [Some(now - AGREED_WITHIN), p.read_back_at, p.agreed_at]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(now - AGREED_WITHIN);
        let replies: Vec<_> =
            super::orchestrator::dm_talks(service, &person, now - AGREED_WITHIN, MESSAGES_READ)
                .into_iter()
                .map(|talk| goal::replied_to(&talk.theirs, &talk.boss, quote, &words, since))
                .collect();
        let unclear = replies.iter().find_map(|r| match r {
            Err(e @ (NotReplied::AsksMore | NotReplied::NotProposed(_) | NotReplied::Says(_))) => {
                Some(*e)
            }
            _ => None,
        });
        let q = crew::one_line(quote, 120);
        match (replies.into_iter().filter_map(Result::ok).max(), unclear) {
            (Some(at), _) => Ok(Some(at)),
            (None, Some(NotReplied::NotProposed(n))) => Err(format!(
                "The person's “{q}” answers your last message before it, which doesn't say {} “{}” word for word. Pass it exactly as you wrote it there; if you mean something else, put that to them in plain words first.",
                proposed[n].0,
                crew::one_line(proposed[n].1, 120)
            )),
            (None, Some(NotReplied::Says(says))) => Err(not_yes(&q, says)),
            (None, Some(_)) => Err(format!(
                "The person's “{q}” answers your last message before it, which asked more than this change, so it's unclear what they agreed to. Ask them about this change on its own, in plain words, then pass their reply."
            )),
            (None, None) => Ok(None),
        }
    }

    /// Notes that question `item`'s proposal applies now, and closes it:
    /// whether this did, so one answer is applied once.
    pub(super) async fn ask_applied(&self, item: Uuid) -> Result<bool, String> {
        self.desk
            .store()
            .ask_applied(item)
            .await
            .map_err(|e| format!("The question answered couldn't be noted: {e}"))
    }

    /// Lets question `item` be answered again: what it was to apply didn't.
    pub(super) async fn ask_unapplied(&self, item: Uuid) {
        if let Err(e) = self.desk.store().ask_unapplied(item).await {
            tracing::warn!(%item, error = %e, "a goal question wasn't let go after its change failed");
        }
    }

    /// One reading of each measured part of `r`, agreed at `agreed_at`, as
    /// the person in scope: refused in the boss's words when a part names
    /// what GitHub hasn't got for them.
    pub(super) async fn first_readings(
        &self,
        service: &AgentService,
        r: &GoalResult,
        agreed_at: DateTime<Utc>,
    ) -> Result<Vec<Reading>, String> {
        let mut out = Vec::new();
        for (i, read) in self.read_parts(service, r, agreed_at).await {
            match read {
                Ok(reading) => out.push(reading),
                Err(measure::Unreadable(why)) => return Err(format!("Part {}: {why}", i + 1)),
            }
        }
        Ok(out)
    }

    /// Each measured part of `r`, agreed at `agreed_at`, read now as the
    /// person in scope, by its place.
    async fn read_parts(
        &self,
        service: &AgentService,
        r: &GoalResult,
        agreed_at: DateTime<Utc>,
    ) -> Vec<(usize, Result<Reading, measure::Unreadable>)> {
        let at = crew::now();
        let access = access(service);
        self.as_person(service, async {
            let mut out = Vec::new();
            for (i, part) in r.parts.iter().enumerate().filter(|(_, p)| p.measured()) {
                let n = u16::try_from(i).unwrap_or_default();
                let read = measure::read(&access, &self.jobs_read, part, n, agreed_at, at);
                // GitHub slow is a failed reading: backed off and counted as
                // any other, the parts read before it kept.
                let read = tokio::time::timeout(READ_FOR, read)
                    .await
                    .unwrap_or_else(|_| Ok(Reading::failed(n, at, SLOW)));
                out.push((i, read));
            }
            out
        })
        .await
    }

    /// Plan `p`'s goal read now and kept, once it reads met kept met, as a
    /// plan turn reads it: the plan fresh, none when it isn't read.
    pub(super) async fn goal_reread(
        &self,
        service: &AgentService,
        p: &PlanDetail,
    ) -> Option<PlanDetail> {
        let fresh = self.read_kept(service, p).await?;
        let (Some(r), Some(progress)) = (&fresh.summary.plan.result, &fresh.goal) else {
            return Some(fresh);
        };
        if !goal::met(progress) {
            return Some(fresh);
        }
        let id = fresh.summary.plan.id;
        if let Err(e) = self
            .desk
            .goal_met(id, &met_by_reading(r, progress, crew::now()))
            .await
        {
            tracing::warn!(plan = %id, error = %e, "a goal read met wasn't kept met");
        }
        self.desk.plan(id).await.ok()
    }

    /// Plan `p`'s measured parts read now as the person and kept: the plan
    /// fresh, none when it isn't read (no goal agreed, met already, not
    /// running or paused, or nothing measured).
    pub(super) async fn read_kept(
        &self,
        service: &AgentService,
        p: &PlanDetail,
    ) -> Option<PlanDetail> {
        use workforce_desk::plan::PlanState;
        let plan = &p.summary.plan;
        let r = plan.result.as_ref()?;
        let reads = plan.met.is_none()
            && plan.approved_revision.is_some()
            && matches!(plan.state, PlanState::Running | PlanState::Paused)
            && r.parts.iter().any(Measure::measured);
        if !reads {
            return None;
        }
        let agreed_at = plan.agreed_at.unwrap_or(plan.created_at);
        let at = crew::now();
        let readings: Vec<Reading> = self
            .read_parts(service, r, agreed_at)
            .await
            .into_iter()
            .map(|(i, read)| match read {
                Ok(reading) => reading,
                Err(measure::Unreadable(why)) => {
                    Reading::failed(u16::try_from(i).unwrap_or_default(), at, &why)
                }
            })
            .collect();
        self.keep_readings(plan.id, r, &readings).await;
        self.desk.plan(plan.id).await.ok()
    }

    /// `read` run as the person `service` works for: what GitHub is read
    /// as, whoever's turn it is.
    pub(super) async fn as_person<T>(
        &self,
        service: &AgentService,
        read: impl std::future::Future<Output = T>,
    ) -> T {
        match self.person(Some(service)) {
            Some(person) => {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(person, read)
                    .await
            }
            None => read.await,
        }
    }

    /// Keeps `readings` of plan `plan`'s result `r`.
    pub(super) async fn keep_readings(&self, plan: Uuid, r: &GoalResult, readings: &[Reading]) {
        for reading in readings {
            let lower = r
                .parts
                .get(usize::from(reading.part))
                .is_some_and(Measure::lower_is_nearer);
            if let Err(e) = self.desk.store().keep_reading(plan, reading, lower).await {
                tracing::warn!(%plan, error = %e, "a goal's reading wasn't kept");
            }
        }
    }

    /// Says plan `p`'s goal back to the person once, in `service`'s DM, as
    /// it stands now, and notes that it was said.
    pub(super) async fn read_back(&self, service: &Arc<AgentService>, p: &PlanDetail) {
        let plan = &p.summary.plan;
        let (Some(r), Some(progress)) = (&plan.result, &p.goal) else {
            return;
        };
        let text = goal::read_back(&plan.goal, r, progress);
        self.post_plan(service, plan, &text).await;
        if let Err(e) = self.desk.store().set_read_back(plan.id, crew::now()).await {
            tracing::warn!(plan = %plan.id, error = %e, "a goal's read-back wasn't noted");
        }
    }

    /// Plan `id`'s goal met as `met` says: kept once, its ticket moved to
    /// Done by Desk, and its closing turn, which finishes it, run after on
    /// its own as any plan turn is: never inside the call that met it, and
    /// only once a plan paused runs again.
    pub(super) async fn goal_met(&self, id: Uuid, met: Met) -> Result<(), String> {
        let why = met.why.clone();
        let first = self
            .desk
            .goal_met(id, &met)
            .await
            .map_err(|e| format!("The goal couldn't be marked met: {e}"))?;
        // Paused, its closing turn waits for it to run again: the
        // watchdog's next look then.
        let running = self
            .desk
            .plan(id)
            .await
            .is_ok_and(|p| p.summary.plan.state == workforce_desk::plan::PlanState::Running);
        if first && running {
            self.close_met(id, &why);
        }
        Ok(())
    }

    /// The goal met now when every part of a first reading met it.
    pub(super) async fn met_at_once(&self, p: &PlanDetail) {
        let plan = &p.summary.plan;
        if plan.met.is_some() || !p.goal.as_ref().is_some_and(goal::met) {
            return;
        }
        let met = Met {
            at: crew::now(),
            by: MetBy::Result,
            why: "every part of it read met as it was saved".into(),
        };
        if let Err(e) = self.goal_met(plan.id, met).await {
            tracing::warn!(plan = %plan.id, error = %e, "a goal met as it was saved wasn't closed");
        }
    }

    /// Keeps what boss `by` recorded on plan `plan`, each checked against
    /// what Desk can find of its evidence: what it kept, by id.
    pub(super) async fn keep_recorded(
        &self,
        service: &AgentService,
        plan: Uuid,
        recorded: Vec<Recorded>,
        by: &Party,
    ) -> Result<Vec<(Uuid, bool)>, String> {
        let mut kept = Vec::with_capacity(recorded.len());
        for r in recorded {
            let checked = r.shown || self.evidence_found(service, &r.evidence).await;
            let f = Fact {
                id: Uuid::new_v4(),
                plan,
                kind: r.kind,
                part: r.part,
                at: crew::now(),
                text: r.said,
                evidence: r.evidence,
                checked,
                made_by: by.clone(),
                replaces: r.replaces,
            };
            self.desk
                .store()
                .keep_fact(&f)
                .await
                .map_err(|e| format!("A fact couldn't be kept: {e}"))?;
            kept.push((f.id, checked));
        }
        Ok(kept)
    }

    /// Whether every piece of `evidence` is found as it says: a run or a
    /// pull request on GitHub, as the person; a ticket on Desk's boards. A
    /// commit Desk can't look up alone, so evidence naming one stays
    /// unchecked.
    async fn evidence_found(&self, service: &AgentService, evidence: &[Evidence]) -> bool {
        if evidence.is_empty() {
            return false;
        }
        let access = access(service);
        self.as_person(service, async {
            for e in evidence {
                let found = match e {
                    Evidence::Run { repo, id } => {
                        github_found(&access, repo, "github.workflow_run_logs", "run_id", *id).await
                    }
                    Evidence::Pull { repo, number } => {
                        github_found(&access, repo, "github.get_pull", "number", *number).await
                    }
                    Evidence::Ticket { key } => self.desk.ticket_by_key(key).await.is_ok(),
                    Evidence::Commit { .. } => false,
                };
                if !found {
                    return false;
                }
            }
            true
        })
        .await
    }

    /// `boss`'s open goals and the newest facts and checkpoints on them,
    /// as every turn of its is given them.
    pub(super) async fn goals_block(&self, boss: &str) -> Option<String> {
        let goals = match self.desk.store().open_goals(Some(boss)).await {
            Ok(g) => g,
            Err(e) => {
                tracing::info!(error = %e, "a boss's goals couldn't be read for its turn");
                return None;
            }
        };
        if goals.is_empty() {
            return None;
        }
        let ids: Vec<Uuid> = goals.iter().map(|p| p.id).collect();
        let facts = self.desk.store().facts(&ids).await.unwrap_or_default();
        let mut standing = Vec::with_capacity(goals.len());
        for p in goals {
            let readings = self.desk.store().readings(p.id).await.unwrap_or_default();
            let own: Vec<Fact> = facts.iter().filter(|f| f.plan == p.id).cloned().collect();
            let progress = p
                .result
                .as_ref()
                .map(|r| goal::progress(r, &readings, &own))
                .unwrap_or_default();
            standing.push((p, progress));
        }
        goal::goals_block(&standing, &facts)
    }

    /// The sessions boss `service` started or was handed and sent work to
    /// as the person asked for it, in the last day, with no agreed goal
    /// holding them: a line each, for its turn's context and its nudges.
    pub(super) async fn unsaved_lines(&self, service: &AgentService) -> Vec<(Uuid, String)> {
        use workforce_desk::charge::How;
        let boss = service.agent_id.as_str();
        let runs: Vec<Uuid> = self
            .desk
            .charges()
            .into_iter()
            .filter(|c| c.boss == boss && matches!(c.how, How::Started | How::Handed))
            .map(|c| c.session)
            .collect();
        if runs.is_empty() {
            return Vec::new();
        }
        let Some(person) = self.person(Some(service)) else {
            return Vec::new();
        };
        let since = crew::now() - AGREED_WITHIN;
        let turns: Vec<DateTime<Utc>> =
            super::orchestrator::person_messages(service, &person, since, MESSAGES_READ)
                .into_iter()
                .map(|(at, _)| at)
                .collect();
        let sends = self.boss_sends(boss, &runs, since).await;
        let on_goals = self.goal_sessions(boss).await;
        goal::unsaved_goals(&runs, &sends, &turns, &on_goals)
            .into_iter()
            .map(|s| (s, unsaved_line(&self.session_name(s), s)))
            .collect()
    }

    /// When boss `boss` sent each of `sessions` a line message since
    /// `since`.
    async fn boss_sends(
        &self,
        boss: &str,
        sessions: &[Uuid],
        since: DateTime<Utc>,
    ) -> Vec<(Uuid, DateTime<Utc>)> {
        match self.desk.store().line_sent_by(boss, sessions, since).await {
            Ok(s) => s,
            Err(e) => {
                tracing::info!(error = %e, "what a boss sent its sessions couldn't be read");
                Vec::new()
            }
        }
    }

    /// The sessions boss `boss`'s agreed open goals hold: their tickets'
    /// sessions. A draft holds none: saving the goal the person agreed is
    /// what does.
    async fn goal_sessions(&self, boss: &str) -> HashSet<Uuid> {
        let mut out = HashSet::new();
        for p in self
            .desk
            .store()
            .open_goals(Some(boss))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.approved_revision.is_some())
        {
            for t in self.desk.plan_tickets(p.id).await.unwrap_or_default() {
                out.extend(
                    t.session
                        .as_deref()
                        .and_then(|s| s.rsplit('/').next()?.parse::<Uuid>().ok()),
                );
            }
        }
        out
    }

    /// Why a boss may not move `ticket` (`key`) to `to`: it stands for a
    /// goal that isn't met, and Desk moves it to Done when it is.
    pub(super) async fn goal_held(
        &self,
        ticket: Uuid,
        key: &str,
        to: workforce_desk::board::TicketState,
    ) -> Option<String> {
        use workforce_desk::board::TicketState;
        if !matches!(to, TicketState::Done | TicketState::Dropped) {
            return None;
        }
        let p = match self.desk.store().open_goal_of_ticket(ticket).await {
            Ok(p) => p?,
            Err(e) => {
                tracing::warn!(%ticket, error = %e, "whether a ticket stands for a goal couldn't be read");
                return None;
            }
        };
        let r = p.result.as_ref()?;
        let readings = self.desk.store().readings(p.id).await.unwrap_or_default();
        let facts = self.desk.store().facts(&[p.id]).await.unwrap_or_default();
        let progress = goal::progress(r, &readings, &facts);
        Some(held_line(key, &p, &goal::progress_line(r, &progress)))
    }
}

/// Whether GitHub has `what` (`field` `id`) of `repo` for the person.
async fn github_found(
    access: &GithubAccess<'_>,
    repo: &str,
    call: &str,
    field: &str,
    id: u64,
) -> bool {
    let Some((owner, name)) = goal::github_repo(repo) else {
        return false;
    };
    let mut args = json!({ "owner": owner, "repo": name });
    args[field] = json!(id);
    super::github::calls_as_person(access, vec![(call, args)])
        .await
        .is_ok()
}

/// What the boss is told of a session it sent work and saved no goal for.
pub(super) fn unsaved_line(name: &str, session: Uuid) -> String {
    format!(
        "{name} (session {session}) has work from you and no agreed goal saved: if the person agreed a goal for it, save it with crew.plan (agreed: their words from the last 24 hours, or ask them to confirm older ones; result), adopting the session with tickets: [{{session: \"{session}\"}}]."
    )
}

/// Why the boss can't move a goal's ticket on, and what it does instead:
/// "MAN-768 stands for the goal “…”, which isn't met (…); Desk moves it to
/// Done when the goal is met. …"
pub(super) fn held_line(key: &str, p: &Plan, progress: &str) -> String {
    format!(
        "{key} stands for the goal “{}”, which isn't met ({progress}); Desk moves it to Done when the goal is met. Keep the work going toward it; if you think it is done, ask the person with what shows it (crew.ask_person with plan_id {} and proposal: {{met: \"<what shows it>\"}}), then pass their reply with crew.plan_update met: {{person_said}}.",
        crew::one_line(&p.goal, 160),
        p.id
    )
}

/// What a plan turn says of plan `plan` whose tickets are all settled
/// while its goal isn't met.
pub(super) fn settled_unmet(plan: Uuid, progress: &str) -> String {
    format!(
        "Every ticket is settled but the goal isn't met: {progress}. Add the work it needs with crew.plan_update (plan_id {plan}, tickets; the person's agreement covers it). If you think it is met, ask them with what shows it: crew.ask_person with plan_id {plan} and proposal: {{met: \"<what shows it>\"}}, then pass their reply with crew.plan_update met: {{person_said}}; a short “yes” counts."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged() -> Value {
        json!({ "words": "the services a change touches are the ones tested",
                "parts": [{ "kind": "judged", "evidence": "the session's commits and CI" }] })
    }

    #[test]
    fn a_result_is_read_and_checked() {
        assert!(result_of(&judged()).is_ok());
        let bad = json!({ "words": "", "parts": [] });
        assert_eq!(
            result_of(&bad).unwrap_err(),
            "A result has 1 to 4 parts. Say what done is in words (words), 1 to 300 characters."
        );
        assert!(
            result_of(&json!({ "words": "x", "parts": [{ "kind": "metric" }] }))
                .unwrap_err()
                .starts_with("The result doesn't read")
        );
    }

    #[test]
    fn a_proposal_says_something() {
        assert!(proposal_of(&json!({})).is_err());
        let p = proposal_of(&json!({ "result": judged() })).unwrap();
        assert!(p.result.is_some() && p.goal.is_none());
        assert_eq!(
            proposal_of(&json!({ "met": "run 12 took 4 minutes" }))
                .unwrap()
                .met
                .as_deref(),
            Some("run 12 took 4 minutes")
        );
    }

    #[test]
    fn facts_and_a_checkpoint_are_read_against_the_result() {
        let r = GoalResult {
            words: "fast CI".into(),
            parts: vec![
                Measure::Ci {
                    repo: "acme/api".into(),
                    workflow: None,
                    branch: "main".into(),
                    want: goal::Want::Green { runs: 1 },
                },
                Measure::Judged {
                    evidence: "the touched services".into(),
                },
            ],
        };
        let input = json!({
            "facts": [{ "said": "The gateway is upgraded.", "evidence": [{ "kind": "pull", "repo": "acme/api", "number": 4 }] }],
            "checkpoint": { "part": 2, "said": "Split the workflow", "evidence": [{ "kind": "commit", "repo": "acme/api", "sha": "abc" }] }
        });
        let got = recorded_of(&input, Some(&r)).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].part, Some(1));
        assert_eq!(got[1].kind, FactKind::Checkpoint);
        let measured = json!({ "checkpoint": { "part": 1, "said": "x", "evidence": [{ "kind": "ticket", "key": "ACM-1" }] } });
        assert!(recorded_of(&measured, Some(&r))
            .err()
            .unwrap()
            .contains("is measured"));
        let bare = json!({ "checkpoint": { "part": 2, "said": "x", "evidence": [] } });
        assert!(recorded_of(&bare, Some(&r)).is_err());
        assert!(recorded_of(&input, None)
            .err()
            .unwrap()
            .contains("no result"));
    }

    #[test]
    fn a_repair_waits_for_failed_reads_and_changes_only_the_address() {
        let ci = |repo: &str, runs: u8| Measure::Ci {
            repo: repo.into(),
            workflow: None,
            branch: "main".into(),
            want: goal::Want::Green { runs },
        };
        let r = GoalResult {
            words: "green".into(),
            parts: vec![ci("acme/api", 1)],
        };
        let failing = |n: u32| goal::Progress {
            parts: vec![goal::PartProgress {
                failing: n,
                ..Default::default()
            }],
        };
        let input = |runs: u8| json!({ "repair": { "part": 1, "measure": serde_json::to_value(ci("acme/api-v2", runs)).unwrap() } });
        assert!(repaired(&input(1), &r, &failing(2))
            .unwrap_err()
            .contains("only once"));
        assert_eq!(
            repaired(&input(1), &r, &failing(3)).unwrap().unwrap().parts[0],
            ci("acme/api-v2", 1)
        );
        assert!(repaired(&input(2), &r, &failing(3))
            .unwrap_err()
            .contains("never its kind or bound"));
        assert_eq!(repaired(&json!({}), &r, &failing(3)), Ok(None));
    }

    #[test]
    fn the_lines_say_what_to_do() {
        let s = Uuid::nil();
        assert!(unsaved_line("acme-web", s).starts_with(&format!(
            "acme-web (session {s}) has work from you and no agreed goal saved: if the person agreed a goal for it, save it with crew.plan (agreed: their words from the last 24 hours"
        )));
        let unmet = settled_unmet(s, "CI: 9 minutes");
        assert!(unmet.starts_with(
            "Every ticket is settled but the goal isn't met: CI: 9 minutes. Add the work it needs"
        ));
        assert!(unmet.contains(&format!(
            "crew.ask_person with plan_id {s} and proposal: {{met:"
        )));
        assert!(unmet.contains("crew.plan_update met: {person_said}"));
    }
}
