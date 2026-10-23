//! Keeping a request within its wire's image limit. Images accumulate over a
//! turn — attachments in the task, each `view_attachment` in a tool-result
//! turn, a video as several frames — and every round resends them all. Past
//! the limit the oldest video frames give way first, then the oldest other
//! images, each replaced where it stood by a note saying so. No message or
//! block moves, so tool pairing and cached prefixes hold.

use std::borrow::Cow;

use super::types::{strip_foreign_reasoning, ContentBlock, LlmMessage};

/// Image and document blocks an Anthropic-shaped request carries at most.
/// The Messages API counts up to 100 or 600, but past 20 every image must be
/// at most 2000px a side and a user's photo is never resized; Bedrock
/// Converse refuses more than 20 in a message.
pub const ANTHROPIC_MAX_IMAGES: usize = 20;

/// Image and file parts a Responses request to the ChatGPT backend carries
/// at most. The backend documents no limit and none has been observed, so
/// this bounds what is known instead: every round resends every image, a
/// video is up to 20 frames of up to 1568px a side, and the model's window
/// is 272k tokens. Fifty keeps two full videos and their turn's attachments
/// whole while a request stays in the tens of megabytes.
pub const RESPONSES_MAX_IMAGES: usize = 50;

/// `messages` as an Anthropic-shaped wire (the Messages API direct, on
/// Bedrock and on Vertex, and Bedrock Converse) accepts them: no foreign
/// reasoning, at most [`ANTHROPIC_MAX_IMAGES`] images. Borrows when nothing
/// changes.
pub fn anthropic_shaped(messages: &[LlmMessage]) -> Cow<'_, [LlmMessage]> {
    cap_images(strip_foreign_reasoning(messages), ANTHROPIC_MAX_IMAGES)
}

/// The text block a sampled video frame's image follows.
pub fn frame_label(index: usize, total: usize, filename: &str, at: &str) -> String {
    format!("Frame {index}/{total} of `{filename}` at {at}")
}

fn is_frame_label(text: &str) -> bool {
    text.starts_with("Frame ") && text.contains(" of `") && text.contains("` at ")
}

/// `messages` carrying at most `max` image and document blocks, images
/// dropped to get there: frames before other images, oldest first.
/// Documents count but stay.
pub fn cap_images(messages: Cow<'_, [LlmMessage]>, max: usize) -> Cow<'_, [LlmMessage]> {
    let documents = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::Document { .. }))
        .count();
    let images: Vec<(usize, usize, bool)> = messages
        .iter()
        .enumerate()
        .flat_map(|(m, message)| {
            message
                .content
                .iter()
                .enumerate()
                .filter(|(_, b)| matches!(b, ContentBlock::Image { .. }))
                .map(move |(b, _)| {
                    let labelled = b.checked_sub(1).is_some_and(|prev| {
                        matches!(&message.content[prev], ContentBlock::Text { text } if is_frame_label(text))
                    });
                    (m, b, labelled)
                })
        })
        .collect();
    let excess = (images.len() + documents).saturating_sub(max);
    if excess == 0 {
        return messages;
    }
    let (frames, others): (Vec<_>, Vec<_>) = images.into_iter().partition(|(_, _, f)| *f);
    let mut owned = messages.into_owned();
    for (m, b, frame) in frames.into_iter().chain(others).take(excess) {
        owned[m].content[b] = ContentBlock::Text {
            text: if frame {
                format!(
                    "[video frame removed: this request reached its limit of {max} images; \
                     view the video again to see it]"
                )
            } else {
                format!("[image removed: this request reached its limit of {max} images]")
            },
        };
    }
    tracing::debug!(
        removed = excess,
        max,
        "dropped images past the request limit"
    );
    Cow::Owned(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ImageSource, Role};

    fn image(tag: &str) -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "image/jpeg".into(),
                data: tag.into(),
            },
        }
    }

    fn document() -> ContentBlock {
        ContentBlock::Document {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "application/pdf".into(),
                data: "pdf".into(),
            },
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn user(content: Vec<ContentBlock>) -> LlmMessage {
        LlmMessage {
            role: Role::User,
            content,
        }
    }

    /// `frames` labelled frames of `name`, as `frames::present` renders them.
    fn video(name: &str, frames: usize) -> Vec<ContentBlock> {
        (1..=frames)
            .flat_map(|i| {
                [
                    text(&format!("{}:", frame_label(i, frames, name, "0:01.0"))),
                    image(&format!("{name}#{i}")),
                ]
            })
            .collect()
    }

    fn kept(messages: &[LlmMessage]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Image { source } => Some(source.data.clone()),
                _ => None,
            })
            .collect()
    }

    fn notes(messages: &[LlmMessage]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::Text { text } if text.contains("removed: this request")))
            .count()
    }

    #[test]
    fn a_request_within_the_limit_is_borrowed_untouched() {
        let messages = vec![user(vec![image("photo")]), user(video("a.mp4", 3))];
        assert!(matches!(
            cap_images(Cow::Borrowed(&messages), 4),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn the_oldest_frames_go_before_any_photo() {
        let messages = vec![
            user(vec![image("photo"), text("what is this room?")]),
            user(video("a.mp4", 3)),
            user(video("b.mp4", 3)),
        ];
        let capped = cap_images(Cow::Borrowed(&messages), 5);
        assert_eq!(
            kept(&capped),
            ["photo", "a.mp4#3", "b.mp4#1", "b.mp4#2", "b.mp4#3"]
        );
        assert_eq!(notes(&capped), 2);
        assert!(matches!(
            &capped[1].content[1],
            ContentBlock::Text { text } if text.contains("video frame removed") && text.contains("limit of 5")
        ));
        assert_eq!(
            capped.iter().map(|m| m.content.len()).collect::<Vec<_>>(),
            messages.iter().map(|m| m.content.len()).collect::<Vec<_>>(),
            "blocks are replaced, never removed"
        );
    }

    #[test]
    fn photos_go_oldest_first_once_no_frames_are_left() {
        let messages = vec![
            user(vec![image("old"), image("mid")]),
            user(vec![image("new")]),
        ];
        let capped = cap_images(Cow::Borrowed(&messages), 1);
        assert_eq!(kept(&capped), ["new"]);
        assert!(matches!(
            &capped[0].content[0],
            ContentBlock::Text { text } if text.starts_with("[image removed")
        ));
    }

    #[test]
    fn documents_count_towards_the_limit_but_stay() {
        let messages = vec![user(vec![document(), document(), image("photo")])];
        let capped = cap_images(Cow::Borrowed(&messages), 2);
        assert!(kept(&capped).is_empty());
        assert_eq!(
            capped[0]
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::Document { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn an_anthropic_shaped_request_keeps_the_newest_twenty() {
        let messages: Vec<LlmMessage> = (0..5)
            .map(|i| user(video(&format!("{i}.mp4"), 5)))
            .collect();
        let capped = anthropic_shaped(&messages);
        let kept = kept(&capped);
        assert_eq!(kept.len(), ANTHROPIC_MAX_IMAGES);
        assert_eq!(kept[0], "1.mp4#1");
    }
}
