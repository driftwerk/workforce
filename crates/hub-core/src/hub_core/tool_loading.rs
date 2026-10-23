//! What the tool plan keeps resident for an agent: the tools it calls often
//! enough that loading the rest stays within its load budget, tuned daily from
//! its telemetry, and the tools a conversation used or loaded in its recent
//! turns.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use workforce_agent::tools::loading::{PlanContext, ToolLoading, ToolPlan, LOAD_TOOL};
use workforce_agent::tools::ToolDefinition;
use workforce_common::persistence::ToolTurn;
use workforce_common::session::{MessageRole, SessionMessage};
use workforce_common::settings::ToolLoadingMode;

/// How often an agent's tool usage is read again.
pub const REFRESH_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// More turns than any agent has in a usage window; the query's row cap.
pub const MAX_USAGE_ROWS: usize = 100_000;

/// Fewer turns than this and the agent has no usage to go on: it is offered
/// every tool until it has.
const MIN_TUNING_TURNS: usize = 30;

/// The shares of turns a tool must be called on to stay resident, from any
/// call at all upwards.
const THRESHOLDS: [f64; 7] = [0.0, 0.005, 0.01, 0.02, 0.03, 0.05, 0.10];

/// An agent's tuned core: the tools resident by usage, the share of turns
/// that picked them, the share of held-out turns that needed a load with
/// them, and the tokens the plan they make offers.
#[derive(Debug, Clone, PartialEq)]
pub struct TunedCore {
    pub core: Arc<BTreeSet<String>>,
    pub threshold: f64,
    pub load_rate: f64,
    pub offered_tokens: u32,
}

/// The core that offers the fewest tokens while at most `budget` of turns
/// would need a tool loaded, judged on the later half of `turns` (oldest
/// first) with usage from the earlier half, then taken from all of them;
/// `None` when there are too few turns to judge.
pub fn tune_core(
    turns: &[ToolTurn],
    definitions: &[ToolDefinition],
    always: &Arc<BTreeSet<String>>,
    budget: f64,
) -> Option<TunedCore> {
    if turns.len() < MIN_TUNING_TURNS {
        return None;
    }
    let (earlier, later) = turns.split_at(turns.len() / 2);
    let judged: Vec<TunedCore> = THRESHOLDS
        .iter()
        .map(|&threshold| judge(threshold, earlier, later, definitions, always))
        .collect();
    let chosen = judged
        .iter()
        .filter(|candidate| candidate.load_rate <= budget)
        .min_by_key(|candidate| candidate.offered_tokens)
        .unwrap_or(&judged[0]);
    Some(TunedCore {
        load_rate: chosen.load_rate,
        ..judge(chosen.threshold, turns, turns, definitions, always)
    })
}

fn judge(
    threshold: f64,
    usage: &[ToolTurn],
    scored: &[ToolTurn],
    definitions: &[ToolDefinition],
    always: &Arc<BTreeSet<String>>,
) -> TunedCore {
    let core = Arc::new(core_at(usage, threshold));
    let loading = ToolLoading {
        mode: ToolLoadingMode::On,
        core: Some(Arc::clone(&core)),
        always: Arc::clone(always),
    };
    let session = BTreeSet::new();
    let plan = ToolPlan::build(
        definitions.to_vec(),
        &PlanContext {
            loading: &loading,
            session: &session,
            context_floor: None,
            precurated: false,
        },
    );
    let loads = scored
        .iter()
        .filter(|turn| turn.tools.iter().any(|tool| plan.is_unloaded(tool)))
        .count();
    TunedCore {
        core,
        threshold,
        load_rate: loads as f64 / scored.len().max(1) as f64,
        offered_tokens: plan.offered_tokens(),
    }
}

/// The tools called on at least `threshold` of `turns`; any call at all for
/// a threshold of zero.
fn core_at(turns: &[ToolTurn], threshold: f64) -> BTreeSet<String> {
    let calls = turns.iter().flat_map(|turn| turn.tools.iter()).fold(
        HashMap::<&str, usize>::new(),
        |mut calls, tool| {
            *calls.entry(tool).or_default() += 1;
            calls
        },
    );
    let floor = threshold * turns.len() as f64;
    calls
        .into_iter()
        .filter(|&(_, count)| count as f64 >= floor)
        .map(|(tool, _)| tool.to_string())
        .collect()
}

/// The tools a session called successfully or loaded in its last
/// `keep_turns` user turns, newest turn first.
pub fn session_tools(messages: &[SessionMessage], keep_turns: usize) -> BTreeSet<String> {
    let turn_starts = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == MessageRole::User)
        .map(|(index, _)| index);
    let from = turn_starts.take(keep_turns).last().unwrap_or(0);
    messages[from..]
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .filter_map(|message| message.tool_data.as_ref())
        .flat_map(tools_of_row)
        .collect()
}

/// The tools one tool row made resident: those its output says it loaded,
/// and the tool itself when it ran successfully.
fn tools_of_row(row: &serde_json::Value) -> Vec<String> {
    let loaded = row["output"]["loaded"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string);
    let ran = row["name"]
        .as_str()
        .filter(|name| *name != LOAD_TOOL && row["success"] == true)
        .map(str::to_string);
    loaded.chain(ran).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as Span, Utc};
    use serde_json::json;
    use workforce_common::session::Session;

    fn session() -> Session {
        Session::new("test", "owner", "agent")
    }

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("Does {name}."),
            input_schema: json!({ "type": "object", "properties": { "x": { "type": "string", "description": "y".repeat(400) } } }),
        }
    }

    /// `rare` is called on one turn in `every`; `common` on every turn.
    fn turns(count: usize, every: usize) -> Vec<ToolTurn> {
        (0..count)
            .map(|i| ToolTurn {
                started_at: Utc::now() - Span::minutes((count - i) as i64),
                tools: std::iter::once("common.run".to_string())
                    .chain((i % every == 0).then(|| "rare.run".to_string()))
                    .collect(),
            })
            .collect()
    }

    fn definitions() -> Vec<ToolDefinition> {
        ["common.run", "rare.run"]
            .into_iter()
            .map(def)
            .chain((0..60).map(|i| def(&format!("unused{i}.run"))))
            .collect()
    }

    #[test]
    fn with_too_few_turns_there_is_no_core() {
        assert!(tune_core(&turns(29, 5), &definitions(), &Arc::default(), 0.5).is_none());
        assert!(tune_core(&turns(30, 5), &definitions(), &Arc::default(), 0.5).is_some());
    }

    #[test]
    fn a_rare_tool_leaves_the_core_when_the_budget_allows_its_loads() {
        let history = turns(200, 25);
        let generous = tune_core(&history, &definitions(), &Arc::default(), 0.10).unwrap();
        assert!(!generous.core.contains("rare.run"));
        assert!(generous.core.contains("common.run"));
        assert_eq!(
            generous.load_rate,
            4.0 / 100.0,
            "held-out turns 100..200 call it 4 times"
        );

        let strict = tune_core(&history, &definitions(), &Arc::default(), 0.0).unwrap();
        assert_eq!(strict.threshold, 0.0);
        assert!(strict.core.contains("rare.run"));
        assert_eq!(strict.load_rate, 0.0);
        assert!(strict.offered_tokens > generous.offered_tokens);
    }

    #[test]
    fn a_budget_nothing_meets_falls_back_to_every_used_tool() {
        let mut history = turns(100, 1000);
        history[99].tools.push("unused3.run".to_string());
        let tuned = tune_core(&history, &definitions(), &Arc::default(), 0.0).unwrap();
        assert_eq!(tuned.threshold, 0.0);
        assert!(
            tuned.load_rate > 0.0,
            "no threshold keeps a tool first seen in the held-out half"
        );
        assert!(tuned.core.contains("unused3.run"));
    }

    #[test]
    fn the_estimate_is_the_runners_own_plan() {
        let history = turns(200, 25);
        let tuned = tune_core(&history, &definitions(), &Arc::default(), 0.10).unwrap();
        let loading = ToolLoading {
            mode: ToolLoadingMode::On,
            core: Some(Arc::clone(&tuned.core)),
            always: Arc::default(),
        };
        let session = BTreeSet::new();
        let plan = ToolPlan::build(
            definitions(),
            &PlanContext {
                loading: &loading,
                session: &session,
                context_floor: None,
                precurated: false,
            },
        );
        assert_eq!(plan.offered_tokens(), tuned.offered_tokens);
        assert!(plan.is_unloaded("rare.run"));
    }

    #[test]
    fn a_session_keeps_what_it_ran_and_loaded() {
        let mut s = session();
        s.add_user_message("hi", None);
        s.add_tool_message("github.pr_review", &json!({}), &json!({}), true, 1, None);
        s.add_tool_message("vault.set", &json!({}), &json!(null), false, 1, Some("no"));
        s.add_tool_message(
            LOAD_TOOL,
            &json!({ "names": ["scheduler"] }),
            &json!({ "loaded": ["scheduler"] }),
            true,
            0,
            None,
        );
        s.add_tool_message(
            "reminder",
            &json!({}),
            &json!({ "loaded": ["reminder"] }),
            false,
            0,
            Some("not loaded"),
        );
        assert_eq!(
            session_tools(&s.messages, 10),
            BTreeSet::from([
                "github.pr_review".to_string(),
                "reminder".to_string(),
                "scheduler".to_string(),
            ])
        );
    }

    #[test]
    fn a_tool_unused_for_keep_turns_user_turns_is_dropped() {
        let mut s = session();
        s.add_user_message("turn 1", None);
        s.add_tool_message("github.pr_review", &json!({}), &json!({}), true, 1, None);
        for turn in 2..=4 {
            s.add_user_message(&format!("turn {turn}"), None);
        }
        assert!(session_tools(&s.messages, 4).contains("github.pr_review"));
        assert!(session_tools(&s.messages, 3).is_empty());
        s.add_tool_message("github.pr_review", &json!({}), &json!({}), true, 1, None);
        assert!(session_tools(&s.messages, 1).contains("github.pr_review"));
    }
}
