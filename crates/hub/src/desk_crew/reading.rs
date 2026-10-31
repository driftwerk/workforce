//! Whether a Desk session's last reply asks the person something, read by
//! the overseer's cheap model where Desk's rules can't tell. Desk gives it
//! a few seconds; anything else (no model, an error, an answer that is
//! neither) is no answer, and the turn is done.

use std::sync::Arc;

use workforce_agent::llm::types::{ContentBlock, LlmMessage};
use workforce_agent::runner::with_served_log;
use workforce_common::cost::{CostEntry, TokenCounts};
use workforce_common::subscription::Origin;

use super::CrewHub;
use crate::serve::AgentService;

const SYSTEM: &str = "You read the last message an AI coding assistant wrote when its turn \
ended. Answer YES if it is waiting on the person: it asks them to decide, choose, answer, \
approve, allow or do something before it can go on. Answer NO if it finished, only offers \
more help, or asks only rhetorically. Answer with the one word YES or NO.";

/// How much of a reply is read: its end, where an ask is.
const READ_CHARS: usize = 4000;

/// The model's one word, as an answer.
pub(super) fn verdict(text: &str) -> Option<bool> {
    let word = text
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_uppercase();
    match word.as_str() {
        "YES" => Some(true),
        "NO" => Some(false),
        _ => None,
    }
}

impl CrewHub {
    /// The service whose model reads replies: the overseer's. Without one
    /// nobody has asked for it, so nothing is spent and the rules decide.
    fn reader(&self) -> Option<Arc<AgentService>> {
        self.overseer_service().map(|(s, _)| s)
    }

    pub(super) async fn read_reply(&self, reply: &str) -> Option<bool> {
        let service = self.reader()?;
        let skip = reply.chars().count().saturating_sub(READ_CHARS);
        let tail: String = reply.chars().skip(skip).collect();
        let model = crate::turn_loop::cheap_model(&service);
        let (response, served) = with_served_log(service.llm.create_message(
            Some(SYSTEM),
            &[LlmMessage::user(tail)],
            &[],
            Some(&model),
            Some(8),
            true,
        ))
        .await;
        let response = response
            .inspect_err(|e| tracing::info!(error = %e, "reading a session's reply failed"))
            .ok()?;
        service.cost_tracker.record(CostEntry {
            task_type: Some("desk_reading"),
            description: Some("desk:reading"),
            served,
            ..CostEntry::new(
                service.agent_id.as_str(),
                &model,
                TokenCounts::from(&response.usage),
                Origin::DeskReading,
            )
        });
        let text: String = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        verdict(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::verdict;

    #[test]
    fn only_a_plain_yes_or_no_is_an_answer() {
        assert_eq!(verdict("YES"), Some(true));
        assert_eq!(verdict(" no.\n"), Some(false));
        assert_eq!(verdict("**Yes**"), Some(true));
        assert_eq!(verdict("Probably yes, since it asks"), None);
        assert_eq!(verdict(""), None);
    }
}
