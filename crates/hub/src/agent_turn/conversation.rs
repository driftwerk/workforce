//! The conversation a CLI turn answers in: the transcript it continues, and
//! what of the conversation that transcript has not seen.

use chrono::Utc;
use workforce_agent::llm::types::{ContentBlock, LlmMessage, Role};
use workforce_common::session::{EngineSlot, Session};
use workforce_common::text::truncate_with_ellipsis;

use super::{CliKind, Continuity};
use crate::cli_session::{workspace_lost, RotationPolicy};
use crate::serve::{build_prior_messages, SharedReplay};

/// The most history a catch-up block carries, from the newest back.
const CATCH_UP_BUDGET: usize = 24_000;

/// The conversation a turn answers in: the session whose transcript it
/// continues, and what of its history the asker may see.
pub(crate) struct Conversation<'a> {
    pub session: &'a Session,
    pub replay: Option<&'a SharedReplay>,
    pub continuity: Continuity,
}

/// The transcript a turn's first round continues or starts.
pub(crate) struct Transcript {
    /// `None` for a turn that keeps none.
    pub(super) slot: Option<EngineSlot>,
    pub(super) resume: bool,
}

impl Transcript {
    /// The transcript `conversation` continues on `cli`: none for a one-shot
    /// turn; else the kept one unless it is due for rotation or the workspace
    /// it describes is gone, when a fresh one starts.
    pub(super) fn of(
        conversation: Option<&Conversation<'_>>,
        cli: CliKind,
        rotation: &RotationPolicy,
        fresh_workspace: bool,
    ) -> Self {
        let Some(conversation) = conversation.filter(|c| c.continuity == Continuity::Session)
        else {
            return Self {
                slot: None,
                resume: false,
            };
        };
        match conversation.session.engine_slot(cli.slot_key()) {
            Some(kept)
                if rotation
                    .rotation(kept.turns, kept.last_reply_at, Utc::now())
                    .is_none()
                    && !workspace_lost(fresh_workspace, true, kept.turns) =>
            {
                Self {
                    slot: Some(kept),
                    resume: true,
                }
            }
            _ => Self::fresh(),
        }
    }

    pub(crate) fn fresh() -> Self {
        Self {
            slot: Some(EngineSlot::fresh()),
            resume: false,
        }
    }

    pub(super) fn keeps(&self) -> bool {
        self.slot.is_some()
    }
}

/// What a turn's first round starts from: the conversation the transcript
/// has not seen, then the task.
pub(super) fn opening(
    task: &[ContentBlock],
    transcript: &Transcript,
    conversation: Option<&Conversation<'_>>,
) -> Vec<ContentBlock> {
    let synced_through = transcript
        .slot
        .as_ref()
        .filter(|_| transcript.resume)
        .and_then(|slot| slot.synced_through.as_deref());
    conversation
        .and_then(|c| catch_up(c.session, synced_through, c.replay))
        .map(|text| ContentBlock::Text { text })
        .into_iter()
        .chain(task.iter().map(readable))
        .collect()
}

/// `block` as the CLI's input can carry it: a document, which it takes no
/// block for, becomes its extracted text.
fn readable(block: &ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Document { source } => ContentBlock::Text {
            text: workforce_agent::attachments::pdf::text_of("attachment", &source.data),
        },
        other => other.clone(),
    }
}

/// The conversation since a transcript last saw it, for a CLI that holds
/// none of it: from `synced_through` on — or the last compaction summary,
/// when the transcript is older than that — up to the message being
/// answered, scoped to what `replay`'s viewer may see, newest kept first
/// within [`CATCH_UP_BUDGET`].
pub(crate) fn catch_up(
    session: &Session,
    synced_through: Option<&str>,
    replay: Option<&SharedReplay>,
) -> Option<String> {
    let messages = &session.messages;
    let synced = synced_through.and_then(|id| messages.iter().position(|m| m.id == id));
    let summary = messages.iter().rposition(|m| m.is_compaction_summary());
    let (from, summary) = match (synced, summary) {
        (Some(synced), Some(summary)) if synced > summary => (synced + 1, None),
        (Some(synced), None) => (synced + 1, None),
        (_, Some(summary)) => (summary + 1, Some(messages[summary].content.as_str())),
        (None, None) => (0, None),
    };
    let lines: Vec<String> = build_prior_messages(&messages[from..], replay)
        .iter()
        .rev()
        .filter_map(render_line)
        .scan(0usize, |spent, line| {
            let left = CATCH_UP_BUDGET
                .checked_sub(*spent)
                .filter(|left| *left > 0)?;
            *spent += line.len().min(left);
            Some(match line.len() > left {
                true => truncate_with_ellipsis(&line, left),
                false => line,
            })
        })
        .collect();
    if lines.is_empty() && summary.is_none() {
        return None;
    }
    let history = lines.into_iter().rev().collect::<Vec<_>>().join("\n\n");
    Some(
        [
            "[CONVERSATION SO FAR — what was said in this conversation that you have not seen; \
             the message to answer follows]"
                .to_string(),
        ]
        .into_iter()
        .chain(summary.map(str::to_string))
        .chain((!history.is_empty()).then_some(history))
        .collect::<Vec<_>>()
        .join("\n\n"),
    )
}

/// `message` as a line of the catch-up; `None` when it has nothing to read.
fn render_line(message: &LlmMessage) -> Option<String> {
    let speaker = match message.role {
        Role::User => "user",
        Role::Assistant => "you",
    };
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::Image { .. } => Some("[image]".to_string()),
            ContentBlock::Document { .. } => Some("[document]".to_string()),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| format!("{speaker}: {text}"))
}
