//! The conversation a CLI turn answers in: the transcript it continues, and
//! what of the conversation that transcript has not seen.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use workforce_agent::llm::types::{ContentBlock, LlmMessage, Role};
use workforce_common::channels::is_crew_channel;
use workforce_common::session::{EngineSlot, Session};
use workforce_common::text::truncate_with_ellipsis;

use super::{CliKind, Continuity};
use crate::cli_session::{workspace_lost, RotationPolicy};
use crate::serve::{build_prior_messages, SharedReplay};

/// The most history a catch-up block carries, from the newest back.
const CATCH_UP_BUDGET: usize = 24_000;

/// The shortest passage of a turn's input left out when its transcript
/// already holds it: shorter ones (headings, one-line notes) cost less than
/// the line that would stand for them.
const HELD_PASSAGE_CHARS: usize = 400;

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
    /// The transcript `conversation` continues on `cli` with `model`: none
    /// for a one-shot turn; else the kept one unless it is due for rotation
    /// (a boss's own conversation also on its size) or the workspace it
    /// describes is gone, when a fresh one starts.
    pub(super) fn of(
        conversation: Option<&Conversation<'_>>,
        cli: CliKind,
        model: &str,
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
        let due = |kept: &EngineSlot| match is_crew_channel(&conversation.session.channel) {
            true => rotation.crew_rotation(
                kept.turns,
                kept.last_reply_at,
                kept.context_tokens,
                crate::cli_session::cli_window(model),
                Utc::now(),
            ),
            false => rotation.rotation(kept.turns, kept.last_reply_at, Utc::now()),
        };
        match conversation.session.engine_slot(cli.slot_key()) {
            Some(kept)
                if due(&kept).is_none() && !workspace_lost(fresh_workspace, true, kept.turns) =>
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

    /// The transcript a resumed turn continues; none when it starts afresh.
    fn resumed(&self) -> Option<&EngineSlot> {
        self.slot.as_ref().filter(|_| self.resume)
    }
}

/// What a turn's first round starts from: the conversation the transcript
/// has not seen, then the task, less the passages of it the transcript
/// already holds.
pub(super) fn opening(
    task: &[ContentBlock],
    transcript: &Transcript,
    conversation: Option<&Conversation<'_>>,
) -> Vec<ContentBlock> {
    let resumed = transcript.resumed();
    let holds = resumed.map(|slot| &slot.holds);
    conversation
        .and_then(|c| catch_up(c.session, resumed, c.replay))
        .map(|text| ContentBlock::Text { text })
        .into_iter()
        .chain(task.iter().map(readable).map(|block| match (block, holds) {
            (ContentBlock::Text { text }, Some(holds)) => ContentBlock::Text {
                text: unheld(&text, holds),
            },
            (block, _) => block,
        }))
        .collect()
}

/// `slot` once a turn of `task` was answered in it on `channel`, its last
/// call reading `context_tokens`, the CLI having `compacted` the transcript
/// itself during it or not. A boss's own conversation keeps the passages it
/// was given, to be left out of the next turn's; none once the CLI
/// compacted, or when the size is unknown or fell (a compaction it didn't
/// say).
pub(super) fn answered(
    slot: EngineSlot,
    channel: Option<&str>,
    task: &[ContentBlock],
    context_tokens: u64,
    compacted: bool,
) -> EngineSlot {
    let kept = channel.is_some_and(is_crew_channel)
        && !compacted
        && context_tokens > 0
        && context_tokens >= slot.context_tokens;
    EngineSlot {
        turns: slot.turns + 1,
        last_reply_at: Some(Utc::now()),
        context_tokens,
        holds: match kept {
            true => task
                .iter()
                .map(readable)
                .flat_map(|b| passages_of(&b))
                .collect(),
            false => BTreeSet::new(),
        },
        ..slot
    }
}

/// The hashes of `block`'s passages long enough to leave out.
fn passages_of(block: &ContentBlock) -> Vec<u64> {
    match block {
        ContentBlock::Text { text } => passages(text)
            .filter(|p| p.chars().count() >= HELD_PASSAGE_CHARS)
            .map(passage_hash)
            .collect(),
        _ => Vec::new(),
    }
}

/// `text`'s passages: what blank lines separate.
fn passages(text: &str) -> impl Iterator<Item = &str> {
    text.split("\n\n")
}

fn passage_hash(passage: &str) -> u64 {
    let digest = blake3::hash(passage.as_bytes());
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 of 32 bytes"))
}

/// `text` with each long passage `holds` names standing as a line that says
/// it is unchanged, opening with its own first words.
fn unheld(text: &str, holds: &BTreeSet<u64>) -> String {
    passages(text)
        .map(|p| {
            match p.chars().count() >= HELD_PASSAGE_CHARS && holds.contains(&passage_hash(p)) {
                true => format!(
                    "[Unchanged since it was given earlier in this conversation: “{}”]",
                    truncate_with_ellipsis(p.lines().next().unwrap_or_default().trim(), 80)
                ),
                false => p.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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

/// The conversation since `resumed`, the transcript a turn continues, last
/// saw it, up to the message being answered, scoped to what `replay`'s
/// viewer may see, newest kept first within [`CATCH_UP_BUDGET`]: from the
/// last message it saw, or the ones saved after it once a compaction removed
/// it; for a fresh transcript, from the last compaction summary. "After" is
/// by each message's own saved time: a session's messages are stamped by the
/// one hub that serves it, so the order holds without a clock shared across
/// hubs; the store keeps no sequence that a compaction doesn't renumber.
pub(crate) fn catch_up(
    session: &Session,
    resumed: Option<&EngineSlot>,
    replay: Option<&SharedReplay>,
) -> Option<String> {
    let messages = &session.messages;
    let synced = resumed
        .and_then(|slot| slot.synced_through.as_deref())
        .and_then(|id| messages.iter().position(|m| m.id == id));
    let compacted_after: Option<DateTime<Utc>> = resumed
        .filter(|slot| synced.is_none() && slot.synced_through.is_some())
        .and_then(|slot| slot.synced_at);
    let summary = messages.iter().rposition(|m| m.is_compaction_summary());
    let (from, summary) = match (synced, summary, compacted_after) {
        (Some(synced), Some(summary), _) if synced > summary => (synced + 1, None),
        (Some(synced), None, _) => (synced + 1, None),
        (None, _, Some(at)) => (
            messages
                .iter()
                .position(|m| m.timestamp > at)
                .unwrap_or(messages.len()),
            None,
        ),
        (_, Some(summary), _) => (summary + 1, Some(messages[summary].content.as_str())),
        (None, None, _) => (0, None),
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

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use workforce_common::session::CompactionKind;

    use super::*;

    const CREW: &str = "desk:checkin:boss";

    fn long(word: &str) -> String {
        format!("{word} ").repeat(HELD_PASSAGE_CHARS / 4)
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text {
            text: t.to_string(),
        }
    }

    fn task(parts: &[&str]) -> Vec<ContentBlock> {
        vec![text(&parts.join("\n\n"))]
    }

    fn resumed(slot: EngineSlot) -> Transcript {
        Transcript {
            slot: Some(slot),
            resume: true,
        }
    }

    fn said(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_resumed_crew_transcript_is_given_only_the_passages_it_lacks() {
        let (directives, picture) = (long("directives"), long("picture"));
        let first = task(&["## Heading", &directives, &picture, "wake one"]);
        let slot = answered(EngineSlot::fresh(), Some(CREW), &first, 30_000, false);

        let next = task(&["## Heading", &directives, &long("changed"), "wake two"]);
        let opened = said(&opening(&next, &resumed(slot.clone()), None));

        assert!(!opened.contains(&directives), "{opened}");
        assert!(opened.contains(
            "[Unchanged since it was given earlier in this conversation: “directives directives"
        ));
        assert!(
            opened.contains(&long("changed")),
            "a changed passage is given whole"
        );
        assert!(opened.starts_with("## Heading"), "a short one always is");
        assert!(opened.ends_with("wake two"));

        let fresh = Transcript {
            slot: Some(slot),
            resume: false,
        };
        assert!(
            said(&opening(&next, &fresh, None)).contains(&directives),
            "a fresh transcript is given everything"
        );
    }

    #[test]
    fn what_a_transcript_holds_is_kept_only_while_the_cli_never_compacted_it() {
        let first = task(&[&long("standing"), "wake"]);
        let grown = answered(EngineSlot::fresh(), Some(CREW), &first, 30_000, false);
        assert_eq!(grown.holds.len(), 1, "only the long passage");
        assert_eq!((grown.turns, grown.context_tokens), (1, 30_000));

        let fell = answered(grown.clone(), Some(CREW), &first, 12_000, false);
        assert!(
            fell.holds.is_empty(),
            "its size fell: the CLI compacted it unsaid"
        );
        assert_eq!(fell.context_tokens, 12_000);

        let regrown = answered(grown.clone(), Some(CREW), &first, 155_000, true);
        assert!(
            regrown.holds.is_empty(),
            "compacted during the turn and grown past where it was: still dropped"
        );
        assert!(
            !answered(grown.clone(), Some(CREW), &first, 155_000, false)
                .holds
                .is_empty(),
            "grown with no compaction: kept"
        );

        assert!(
            answered(grown.clone(), Some(CREW), &first, 0, false)
                .holds
                .is_empty(),
            "size unknown"
        );
        assert!(
            answered(grown.clone(), Some("web"), &first, 40_000, false)
                .holds
                .is_empty(),
            "the person's own"
        );
        assert!(answered(grown, None, &first, 40_000, false)
            .holds
            .is_empty());
    }

    /// A session whose transcript saw "old question" and "old answer", then
    /// a compaction removed both, then "asked meanwhile" came, then the
    /// message being answered.
    fn compacted_away() -> (Session, EngineSlot) {
        let mut session = Session::new(CREW, "person", "boss");
        let base = Utc::now() - TimeDelta::minutes(10);
        session.add_user_message("old question", None);
        session.add_assistant_message("old answer", 5);
        for (n, m) in session.messages.iter_mut().enumerate() {
            m.timestamp = base + TimeDelta::seconds(n as i64);
        }
        let last = session.messages.last().unwrap();
        let slot = EngineSlot {
            synced_through: Some(last.id.clone()),
            synced_at: Some(last.timestamp),
            ..EngineSlot::fresh()
        };
        session.compact_with_summary("What was said before.", 0, CompactionKind::Summarisation);
        session.add_user_message("asked meanwhile", None);
        session.add_user_message("now", None);
        (session, slot)
    }

    #[test]
    fn a_resumed_transcript_is_never_given_the_summary_of_what_it_holds() {
        let (session, slot) = compacted_away();
        let caught = catch_up(&session, Some(&slot), None).unwrap();
        assert!(caught.contains("asked meanwhile"), "{caught}");
        assert!(!caught.contains("What was said before."), "{caught}");
        assert!(!caught.contains("old answer"), "{caught}");
    }

    #[test]
    fn a_fresh_transcript_or_one_kept_before_its_message_s_time_was_gets_the_summary() {
        let (session, slot) = compacted_away();
        assert!(catch_up(&session, None, None)
            .unwrap()
            .contains("What was said before."));
        let older = EngineSlot {
            synced_at: None,
            ..slot
        };
        assert!(catch_up(&session, Some(&older), None)
            .unwrap()
            .contains("What was said before."));
    }
}
