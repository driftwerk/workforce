//! The computer gate: on a hub with `[desk]`, what an agent may read,
//! change or run on the person's computer is decided per call by one verdict
//! (`workforce_desk::computer::policy::verdict`), which the hub reaches
//! through [`ComputerGate`]. Every other hub wires none, and nothing here
//! runs.
//!
//! The gate only ever escalates: a call it lets run keeps whatever the other
//! gates decided; a call it asks about is held for the person, offering Allow
//! once and Deny; a call it refuses is denied in a plain sentence. It decides
//! and writes nothing: what came of a call is recorded where it settles
//! ([`ComputerGate::record`]), so a probe or the re-gate of an approved call
//! records nothing.

use std::path::{Path, PathBuf};

use serde_json::Value;
use workforce_core::computer::{Effect, Verdict};
use workforce_core::policy::{PolicyDecision, PolicyResponse};

use crate::tools::paths::{resolve_against_root, resolve_real};
use crate::tools::registry::ToolRegistry;

/// What came of a call the gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It ran without asking.
    Ran,
    /// The verdict refused it.
    Refused,
    /// The person allowed it.
    Allowed,
    /// The person declined it.
    Declined,
    /// Nobody answered in time.
    NotAnswered,
    /// Nobody could be asked: a job, a workflow step, a surface with no card.
    Unattended,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::Refused => "refused",
            Self::Allowed => "allowed",
            Self::Declined => "declined",
            Self::NotAnswered => "not_answered",
            Self::Unattended => "unattended",
        }
    }
}

/// The gate's decision on one call: what the tool said it does, and the
/// verdict on that.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub effect: Effect,
    pub verdict: Verdict,
    /// How far the call reaches past its tool's roots when it runs.
    pub reach: Option<crate::tools::paths::Reach>,
}

/// One settled call, to record.
#[derive(Debug, Clone)]
pub struct Record {
    pub tool: String,
    pub decision: Decision,
    /// The call's input in record form (secret markers intact).
    pub input: Value,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

/// The hub's side of the gate, one per agent.
pub trait ComputerGate: Send + Sync {
    /// Whether `tool` is in a namespace the gate decides. Nothing else is
    /// asked of a tool outside them: its effect is never read.
    fn decides(&self, tool: &str) -> bool;
    /// The verdict on `effect`, made by `tool` in the turn in scope. A
    /// verdict that can't be reached is a refusal.
    fn verdict(&self, tool: &str, effect: &Effect) -> Verdict;
    /// Keeps what came of a call. Must neither block nor fail it.
    fn record(&self, record: Record);
    /// Whether, where nobody can be asked, what would ask runs instead: the
    /// agent is at Full access and its `[computer] unattended` is `run`, the
    /// default, so jobs keep working as before. Never what is refused.
    fn unattended_runs(&self) -> bool {
        false
    }
    /// How far a call with `effect` reaches past its tool's own roots, at
    /// the agent's level: reads anywhere but the withheld set from Read
    /// anywhere up, changes anywhere at Full access. `None` keeps the tool's
    /// roots.
    fn reach(&self, _effect: &Effect) -> Option<crate::tools::paths::Reach> {
        None
    }
}

/// [`decide`] on a path where nobody can be asked: an ask becomes a run
/// when the gate says so ([`ComputerGate::unattended_runs`]); a refusal
/// stays one.
pub fn decide_unattended(
    gate: Option<&dyn ComputerGate>,
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> Option<Decision> {
    let mut decision = decide(gate, tools, tool_name, input)?;
    let asks = matches!(decision.verdict, Verdict::Ask { .. } | Verdict::Review);
    if asks && gate.is_some_and(|g| g.unattended_runs()) {
        decision.verdict = Verdict::Run {
            by: workforce_core::computer::By::Unattended,
        };
    }
    Some(decision)
}

/// The gate's decision on `tool_name` with `input`, when the gate decides it.
pub fn decide(
    gate: Option<&dyn ComputerGate>,
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> Option<Decision> {
    let gate = gate.filter(|gate| gate.decides(tool_name))?;
    let effect = tools
        .get(tool_name)
        .map_or(Effect::Unknown, |tool| tool.effect(input));
    let verdict = gate.verdict(tool_name, &effect);
    let reach = gate.reach(&effect);
    Some(Decision {
        effect,
        verdict,
        reach,
    })
}

/// What a call does, as a card or a refusal names it.
pub fn describe(effect: &Effect) -> String {
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match effect {
        Effect::None => "nothing on this computer".into(),
        Effect::Read { paths } => format!("read {}", list(paths)),
        Effect::Write { paths } => format!("change {}", list(paths)),
        Effect::Delete { paths } => format!("delete {}", list(paths)),
        Effect::Move { from, to } => format!("move {} to {}", list(from), to.display()),
        Effect::Exec { command, cwd } => match cwd {
            Some(cwd) => format!("run `{command}` in {}", cwd.display()),
            None => format!("run `{command}`"),
        },
        Effect::Session { cwd } => match cwd {
            Some(cwd) => format!("start a session in {}", cwd.display()),
            None => "start a session".into(),
        },
        Effect::Connector { connector, write } => match write {
            true => format!("change something in {connector}"),
            false => format!("read from {connector}"),
        },
        Effect::Unknown => "something this tool can't name".into(),
    }
}

/// `response` with the gate's decision laid over it. Only ever escalates.
pub fn apply(response: PolicyResponse, decision: Option<&Decision>) -> PolicyResponse {
    let Some(decision) = decision else {
        return response;
    };
    if matches!(response.decision, PolicyDecision::Deny { .. }) {
        return response;
    }
    match &decision.verdict {
        Verdict::Run { .. } => response,
        Verdict::Refuse { refusal } => PolicyResponse {
            decision: PolicyDecision::Deny {
                reason: refusal.sentence.clone(),
            },
            ..response
        },
        // Working on its own (AC11) is not built yet: a review asks.
        Verdict::Ask { why, .. } => held(response, why.sentence(), &decision.effect),
        Verdict::Review => held(response, "Asks first", &decision.effect),
    }
}

fn held(response: PolicyResponse, why: &str, effect: &Effect) -> PolicyResponse {
    PolicyResponse {
        decision: PolicyDecision::RequireApproval {
            reason: format!("{why}: wants to {}", describe(effect)),
            // Only Allow once and Deny: an "always" here is a grant, kept
            // apart from the permission rules (AC3d).
            allow_save_allow: false,
        },
        ..response
    }
}

/// The answers a computer ask offers: Allow once and Deny. No "always"
/// reaches the permission rules.
pub fn prompt_options(_decision: &Decision) -> workforce_core::policy::PermissionPromptOptions {
    workforce_core::policy::PermissionPromptOptions {
        allow_once: true,
        deny_once: true,
        allow_always: false,
        deny_always: false,
    }
}

/// Why a call that would have asked ran: nobody could be asked.
pub const UNATTENDED_RAN: &str =
    "Nobody could be asked, and at Full access the agent runs it, as before";

/// Records what came of a call the gate decided; nothing when it decided
/// nothing.
pub fn settle(
    gate: Option<&dyn ComputerGate>,
    tool: &str,
    decision: Option<&Decision>,
    input: &Value,
    outcome: Outcome,
    reason: Option<&str>,
) {
    let (Some(gate), Some(decision)) = (gate, decision) else {
        return;
    };
    gate.record(Record {
        tool: tool.to_string(),
        decision: decision.clone(),
        input: input.clone(),
        outcome,
        reason: reason.map(str::to_string),
    });
}

/// What came of a held call the turn's ledger refused.
pub fn refused_as(refusal: crate::tools::approval::Refusal) -> Outcome {
    use crate::tools::approval::Refusal;
    match refusal {
        Refusal::Denied => Outcome::Declined,
        Refusal::Unanswered => Outcome::NotAnswered,
        Refusal::Failed => Outcome::Refused,
        Refusal::Unattended => Outcome::Unattended,
    }
}

/// Records a call as it left the gates: one that runs, or one the verdict
/// refused. A call held for the person is recorded when it settles; one
/// blocked by another check never ran and isn't the gate's.
pub fn settle_gated(
    gate: Option<&dyn ComputerGate>,
    tool: &str,
    decision: Option<&Decision>,
    input: &Value,
    outcome: &crate::tools::dispatch::GateOutcome,
) {
    use crate::tools::dispatch::GateOutcome;
    let Some(d) = decision else { return };
    match (outcome, &d.verdict) {
        (GateOutcome::Execute { .. }, Verdict::Run { by }) => {
            let why = (*by == workforce_core::computer::By::Unattended).then_some(UNATTENDED_RAN);
            settle(gate, tool, decision, input, Outcome::Ran, why)
        }
        (GateOutcome::Blocked { reason, .. }, Verdict::Refuse { .. }) => {
            settle(gate, tool, decision, input, Outcome::Refused, Some(reason))
        }
        _ => {}
    }
}

/// Why a decided call can't run on a path with nobody to ask (a workflow
/// tool step, a job), recording that; `None` when it may run.
pub fn unattended_refusal(
    gate: Option<&dyn ComputerGate>,
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> Option<String> {
    let decision = decide_unattended(gate, tools, tool_name, input)?;
    let gate = gate?;
    let reason = match &decision.verdict {
        Verdict::Run { by } => {
            let reason = (*by == workforce_core::computer::By::Unattended)
                .then(|| UNATTENDED_RAN.to_string());
            gate.record(Record {
                tool: tool_name.to_string(),
                decision,
                input: input.clone(),
                outcome: Outcome::Ran,
                reason,
            });
            return None;
        }
        Verdict::Refuse { refusal } => (Outcome::Refused, refusal.sentence.clone()),
        Verdict::Ask { .. } | Verdict::Review => (
            Outcome::Unattended,
            format!(
                "'{tool_name}' would {} on this computer, which needs the person's say, and \
                 nobody can be asked here, so it did not run.",
                describe(&decision.effect)
            ),
        ),
    };
    gate.record(Record {
        tool: tool_name.to_string(),
        decision,
        input: input.clone(),
        outcome: reason.0,
        reason: Some(reason.1.clone()),
    });
    Some(reason.1)
}

/// Where a tool given `raw` against `roots` would act: absolute and real,
/// as its own resolution makes it.
pub fn path_in(raw: &str, roots: &[PathBuf]) -> PathBuf {
    resolve_real(&resolve_against_root(Path::new(raw.trim()), roots))
}

/// The paths `input[key]` names (a string, or an array of them), resolved
/// against `roots`; the first root when it names none.
pub fn paths_in(input: &Value, key: &str, roots: &[PathBuf]) -> Vec<PathBuf> {
    let named: Vec<&str> = match &input[key] {
        Value::String(s) if !s.trim().is_empty() => vec![s.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if named.is_empty() {
        return roots
            .first()
            .map(|r| vec![resolve_real(r)])
            .unwrap_or_default();
    }
    named.into_iter().map(|raw| path_in(raw, roots)).collect()
}

/// `argv` as a shell would read it back, so the always-asks list sees a
/// runner's command as `exec.run` would show it.
pub fn shell_line<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| {
            let a = a.as_ref();
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,%".contains(c))
            {
                a.to_string()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::computer::{By, Choice, Refusal, Refused, Why};
    use workforce_core::policy::PolicyDecision;

    fn allow() -> PolicyResponse {
        PolicyResponse {
            decision: PolicyDecision::Allow,
            matched_rule: None,
            evaluation_time_us: 0,
        }
    }

    fn decision(verdict: Verdict) -> Decision {
        Decision {
            reach: None,
            effect: Effect::Exec {
                command: "npm test".into(),
                cwd: Some("/w".into()),
            },
            verdict,
        }
    }

    #[test]
    fn the_gate_only_escalates() {
        let run = decision(Verdict::Run { by: By::Rule });
        assert!(matches!(
            apply(allow(), Some(&run)).decision,
            PolicyDecision::Allow
        ));
        assert!(matches!(
            apply(allow(), None).decision,
            PolicyDecision::Allow
        ));
        let ask = decision(Verdict::Ask {
            choices: vec![Choice::AllowOnce, Choice::Deny],
            why: Why::Runs,
        });
        match apply(allow(), Some(&ask)).decision {
            PolicyDecision::RequireApproval {
                reason,
                allow_save_allow,
            } => {
                assert_eq!(
                    reason,
                    "Runs on this computer: wants to run `npm test` in /w"
                );
                assert!(!allow_save_allow);
            }
            other => panic!("{other:?}"),
        }
        let refuse = decision(Verdict::Refuse {
            refusal: Refusal {
                rule: Refused::Stopped,
                sentence: "stopped".into(),
            },
        });
        assert!(matches!(
            apply(allow(), Some(&refuse)).decision,
            PolicyDecision::Deny { reason } if reason == "stopped"
        ));
        let denied = PolicyResponse {
            decision: PolicyDecision::Deny {
                reason: "policy".into(),
            },
            matched_rule: None,
            evaluation_time_us: 0,
        };
        assert!(matches!(
            apply(denied, Some(&ask)).decision,
            PolicyDecision::Deny { reason } if reason == "policy"
        ));
    }

    #[test]
    fn argv_reads_back_as_a_shell_line() {
        assert_eq!(shell_line(&["git", "push", "-f"]), "git push -f");
        assert_eq!(
            shell_line(&["git", "commit", "-m", "it's done"]),
            r"git commit -m 'it'\''s done'"
        );
        assert_eq!(shell_line(&["echo", ""]), "echo ''");
    }
}
