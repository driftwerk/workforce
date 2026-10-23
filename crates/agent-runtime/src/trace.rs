//! Trace capture types for LLM fine-tuning data.
//!
//! This module defines the `TraceWriter` trait and all trace structs used to
//! capture complete `(system_prompt, tools[], messages[])` tuples during agent
//! runs. The trait is abstract (no I/O) — concrete implementations live in
//! downstream crates (e.g. `JsonlTraceWriter` in the hub crate).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::types::{ContentBlock, StopReason};
use crate::tools::ToolDefinition;

/// First line of a trace file — captures the static context for the run.
/// Headers written before the run's origin was recorded carry no `channel`
/// or `request` and empty ids; they still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceHeader {
    pub trace_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub workflow_id: Option<String>,
    pub step_id: Option<String>,
    pub task_type: Option<String>,
    /// The kind of channel the turn came in on (`slack`, `gateway`,
    /// `workflow`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// What a person asked, as the turn received it (secrets already
    /// replaced by their markers): set on a turn that answers an inbound
    /// message while the install records decisions for the agent, absent on
    /// a workflow step or a background run. It is kept and pruned with the
    /// trace it heads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub started_at: DateTime<Utc>,
}

/// Where a run came from and what it is for: what whoever starts it knows
/// and its trace header records.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TraceTags {
    pub workflow_id: Option<String>,
    pub step_id: Option<String>,
    pub task_type: Option<String>,
    pub channel: Option<String>,
    pub request: Option<String>,
}

/// What a person asked is never printed with the rest: only its length.
impl std::fmt::Debug for TraceTags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceTags")
            .field("workflow_id", &self.workflow_id)
            .field("step_id", &self.step_id)
            .field("task_type", &self.task_type)
            .field("channel", &self.channel)
            .field(
                "request_chars",
                &self.request.as_ref().map(|r| r.chars().count()),
            )
            .finish()
    }
}

/// The parts of a header the run itself supplies.
pub struct TraceRun {
    pub trace_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub started_at: DateTime<Utc>,
}

impl TraceTags {
    /// The header of `run`, started with these tags.
    pub fn header(&self, run: TraceRun) -> TraceHeader {
        let TraceTags {
            workflow_id,
            step_id,
            task_type,
            channel,
            request,
        } = self.clone();
        TraceHeader {
            trace_id: run.trace_id,
            agent_id: run.agent_id,
            session_id: run.session_id,
            workflow_id,
            step_id,
            task_type,
            channel,
            request,
            model: run.model,
            system_prompt: run.system_prompt,
            tools: run.tools,
            started_at: run.started_at,
        }
    }
}

/// Per-round delta — captures the new messages exchanged in one LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRound {
    pub round: u32,
    /// The user/task message sent this round (just the new content).
    #[serde(serialize_with = "serialize_blocks")]
    pub input: Vec<ContentBlock>,
    /// The LLM response (full ContentBlocks including thinking, tool_use).
    #[serde(serialize_with = "serialize_blocks")]
    pub response: Vec<ContentBlock>,
    /// Tool results from executing tool calls (before next LLM call).
    pub tool_results: Vec<ToolResultTrace>,
    pub model: String,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
}

/// Blocks as a trace records them: encrypted reasoning (several KB a round,
/// unreadable) becomes its size.
fn serialize_blocks<S: serde::Serializer>(
    blocks: &[ContentBlock],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(blocks.iter().map(|block| {
        match (block, block.oai_reasoning_signature()) {
            (ContentBlock::Thinking { thinking, .. }, Some(signature)) => {
                std::borrow::Cow::Owned(ContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: Some(format!("[encrypted reasoning: {} bytes]", signature.len())),
                })
            }
            _ => std::borrow::Cow::Borrowed(block),
        }
    }))
}

/// Detailed record of a single tool execution within a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultTrace {
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    pub success: bool,
    pub duration_ms: u64,
}

/// Final line of a trace file — captures the outcome and totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFooter {
    pub outcome: TraceOutcome,
    pub completed_at: DateTime<Utc>,
    pub total_rounds: u32,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub total_duration_ms: u64,
}

/// The outcome of an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceOutcome {
    Success,
    MaxRoundsExceeded,
    Error(String),
    Cancelled,
}

/// Abstract writer for trace data. Implementations handle serialization and
/// storage (JSONL files, databases, etc.).
///
/// All methods take `&self` and return `Result` so callers can log-and-ignore
/// failures without affecting the agent run.
pub trait TraceWriter: Send + Sync {
    fn write_header(&self, header: &TraceHeader) -> eyre::Result<()>;
    fn write_round(&self, round: &TraceRound) -> eyre::Result<()>;
    fn write_footer(&self, footer: &TraceFooter) -> eyre::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::OAI_REASONING_PREFIX;

    #[test]
    fn a_header_carries_the_runs_origin_and_one_written_before_it_did_still_loads() {
        let tags = TraceTags {
            workflow_id: Some("triage".into()),
            step_id: Some("classify".into()),
            task_type: Some("research".into()),
            channel: Some("workflow".into()),
            request: None,
        };
        let header = tags.header(TraceRun {
            trace_id: "t1".into(),
            agent_id: "ops".into(),
            session_id: "s1".into(),
            model: "m".into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            started_at: Utc::now(),
        });
        let json = serde_json::to_value(&header).unwrap();
        assert_eq!(json["workflow_id"], "triage");
        assert_eq!(json["step_id"], "classify");
        assert_eq!(json["task_type"], "research");
        assert_eq!(json["channel"], "workflow");
        assert_eq!(json["session_id"], "s1");
        assert!(json.get("request").is_none());

        let old: TraceHeader = serde_json::from_value(serde_json::json!({
            "trace_id": "t0", "agent_id": "ops", "session_id": "", "workflow_id": null,
            "step_id": null, "task_type": null, "model": "m", "system_prompt": "",
            "tools": [], "started_at": "2026-09-01T10:00:00Z",
        }))
        .unwrap();
        assert_eq!((old.channel, old.request), (None, None));
    }

    #[test]
    fn oai_reasoning_signature_is_traced_as_its_size() {
        let signature = format!("{OAI_REASONING_PREFIX}v1:{}", "x".repeat(2000));
        let round = TraceRound {
            round: 1,
            input: vec![ContentBlock::Text { text: "hi".into() }],
            response: vec![
                ContentBlock::Thinking {
                    thinking: "**Planning**".into(),
                    signature: Some(signature.clone()),
                },
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("EqQBCkYIAxgCIkD".into()),
                },
            ],
            tool_results: vec![],
            model: "gpt-6.1-sol".into(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let json = serde_json::to_value(&round).unwrap();
        assert_eq!(
            json["response"][0]["signature"],
            format!("[encrypted reasoning: {} bytes]", signature.len())
        );
        assert_eq!(json["response"][0]["thinking"], "**Planning**");
        assert_eq!(json["response"][1]["signature"], "EqQBCkYIAxgCIkD");
    }
}
