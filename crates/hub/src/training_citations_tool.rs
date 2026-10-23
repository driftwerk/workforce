//! `training_citations` — return the training documents the agent
//! cited in its previous reply, with scores and locators.
//!
//! The agent invokes this tool when the user asks "what did you cite?",
//! "show me your sources", "where did that come from?", or any similar
//! request to surface the documents that backed the previous answer.
//! The store is populated by `compose_prompt` each turn (see
//! `serve::AgentService::compose_prompt`); the tool just reads.

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::runner::CURRENT_SESSION_KEY;
use workforce_agent::tools::{Tool, ToolResult};

use crate::hub_core::session_citations::SessionCitationStore;

const TOOL_NAME: &str = "training_citations";

pub struct TrainingCitationsTool {
    store: SessionCitationStore,
}

impl TrainingCitationsTool {
    pub fn new(store: SessionCitationStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for TrainingCitationsTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Return the training documents you cited in your most recent prior reply, with their \
         relevance scores and locators (page, section, sheet, etc). Call this when the human \
         asks `what did you cite?`, `show me your sources`, `where did that come from?`, or \
         any similar request to surface the documents that backed your previous answer. \
         \n\nReturns `{ citations: [...], note }`. If the list is empty, your previous reply \
         didn't draw on training material — relay that directly rather than guessing. Each \
         citation includes `filename`, `corpus`, `locator`, `score` (0–1), `snippet` (a short \
         excerpt), and `short_id` (the 8-char id used in `[[cite:...]]` markers)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(session_key) = task_local(&CURRENT_SESSION_KEY) else {
            return Ok(ToolResult::failure(
                "training_citations can only be used inside a session",
                elapsed(),
            ));
        };

        let citations = self.store.previous_turn(&session_key).await;
        let (note, items) = match citations {
            Some(c) if !c.is_empty() => {
                let count = c.len();
                (
                    format!("Cited {count} document(s) in my previous reply."),
                    c.into_iter()
                        .map(|c| {
                            serde_json::json!({
                                "filename": c.filename,
                                "corpus": c.corpus,
                                "locator": c.locator,
                                "score": c.score,
                                "snippet": c.snippet,
                                "short_id": c.short_id,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
            }
            _ => (
                "No training material was cited in my previous reply.".to_string(),
                Vec::new(),
            ),
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "citations": items,
                "note": note,
            }),
            elapsed(),
        ))
    }
}

fn task_local(key: &'static tokio::task::LocalKey<String>) -> Option<String> {
    key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
}
