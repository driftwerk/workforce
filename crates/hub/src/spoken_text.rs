//! A spoken turn's reply, parted into what is read and what is heard.
//!
//! An agent answering aloud writes voice directions into its text: `(excited)
//! Found it!`. They are for the voice alone. Everything that keeps or shows the
//! reply (the client's screen, the mid-turn mirror, the saved message, what the
//! model is given next turn, search, notifications) has the text without them;
//! the directions the turn's voice performs travel beside it, as
//! [`ProgressEvent::Speech`] and then a frame's `speech`. See
//! [`workforce_agent::llm::speech::Markup`].

use std::borrow::Cow;

use crate::chat_worker::AnswerDelivery;
use tokio::sync::mpsc;
use workforce_agent::llm::speech::{without_named_directions, Markup, MarkupSplit, Parted};
use workforce_agent::runner::ProgressEvent;
use workforce_common::session::{MessageRole, Session};

/// How `session`'s turn now running is heard; `None` for one that isn't.
pub fn spoken_with(session: &Session) -> Option<Markup> {
    session.is_voice_turn().then(|| {
        let provider = session
            .metadata
            .get("tts_provider")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        Markup::of(provider, session.tts_model())
    })
}

/// The text a spoken turn's answer is kept and shown as; `None` for a turn
/// that wasn't spoken, whose text is as written.
pub fn kept_answer(session: &Session, answer: &str) -> Option<String> {
    spoken_with(session).map(|_| Markup::Plain.apply(answer))
}

/// Whether a voice that takes directions has spoken in `session`, so what was
/// saved in it before they were parted from the text may carry them.
fn may_keep_directions(session: &Session) -> bool {
    session
        .metadata
        .get("tts_provider")
        .and_then(|v| v.as_str())
        .is_some_and(|provider| Markup::of(provider, None) != Markup::Plain)
}

/// A message kept in `session`, as it reads now.
pub fn kept_text<'a>(session: &Session, text: &'a str) -> Cow<'a, str> {
    if may_keep_directions(session) {
        Cow::Owned(without_named_directions(text))
    } else {
        Cow::Borrowed(text)
    }
}

/// What `session` keeps of its agent, as it reads now, for the model to read
/// next: what it sees of itself is what the person saw.
pub fn clean_kept(session: &mut Session) {
    if !may_keep_directions(session) {
        return;
    }
    for message in &mut session.messages {
        if message.role == MessageRole::Assistant {
            message.content = without_named_directions(&message.content);
        }
    }
}

/// Parts a stream of a spoken turn's progress.
#[derive(Debug)]
struct Parting {
    split: MarkupSplit,
    markup: Markup,
    delivery: AnswerDelivery,
    /// Speech of pieces that show nothing yet, for the next frame.
    carried: String,
}

impl Parting {
    fn new(markup: Markup) -> Self {
        Self {
            split: markup.split(),
            markup,
            delivery: AnswerDelivery::default(),
            carried: String::new(),
        }
    }

    /// `event` as the events to pass on.
    fn event(&mut self, event: ProgressEvent) -> Vec<ProgressEvent> {
        self.delivery.note(&event);
        match event {
            ProgressEvent::StreamDelta { text } => {
                let parted = self.split.push(&text);
                self.delta(parted, false)
            }
            ProgressEvent::StreamEnd => {
                let parted = self.split.finish();
                let mut out = self.delta(parted, true);
                out.push(ProgressEvent::StreamEnd);
                out
            }
            ProgressEvent::StreamDiscard { reason } => {
                self.split.reset();
                self.carried.clear();
                vec![ProgressEvent::StreamDiscard { reason }]
            }
            ProgressEvent::Response { text } => {
                let shown = Markup::Plain.apply(&text);
                let spoken = self.markup.apply(&text);
                // Already streamed, it reaches no client: no frame to go with.
                let mut out = if self.delivery.delivers_response() {
                    speech_of(&shown, spoken)
                } else {
                    Vec::new()
                };
                out.push(ProgressEvent::Response { text: shown });
                out
            }
            ProgressEvent::CheckIn { note } => {
                let shown = Markup::Plain.apply(&note);
                let spoken = self.markup.apply(&note);
                let mut out = speech_of(&shown, spoken);
                out.push(ProgressEvent::CheckIn { note: shown });
                out
            }
            other => vec![other],
        }
    }

    /// One parted piece: its speech, if it differs, then what's shown. A piece
    /// that shows nothing waits for one that does, or for the end, where an
    /// empty delta carries it.
    fn delta(&mut self, parted: Parted, last: bool) -> Vec<ProgressEvent> {
        let Parted { shown, spoken } = parted;
        self.carried.push_str(&spoken);
        if shown.is_empty() && (!last || self.carried.is_empty()) {
            return Vec::new();
        }
        let spoken = std::mem::take(&mut self.carried);
        let mut out = speech_of(&shown, spoken);
        out.push(ProgressEvent::StreamDelta { text: shown });
        out
    }
}

/// `spoken` as the event that says so, when it differs from what's shown.
fn speech_of(shown: &str, spoken: String) -> Vec<ProgressEvent> {
    if spoken == shown {
        Vec::new()
    } else {
        vec![ProgressEvent::Speech { text: spoken }]
    }
}

/// `progress` with a turn's reply parted for a voice with `markup`.
pub fn parted(
    mut progress: mpsc::Receiver<ProgressEvent>,
    markup: Markup,
) -> mpsc::Receiver<ProgressEvent> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut parting = Parting::new(markup);
        while let Some(event) = progress.recv().await {
            for event in parting.event(event) {
                if tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// The speech a frame says aloud instead of its text, from the
/// [`ProgressEvent::Speech`] just before it.
#[derive(Debug, Default)]
pub struct NextSpeech(Option<String>);

impl NextSpeech {
    pub fn set(&mut self, text: String) {
        self.0 = Some(text);
    }

    pub fn take(&mut self) -> Option<String> {
        self.0.take()
    }

    pub fn clear(&mut self) {
        self.0 = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::StreamDiscardReason;

    fn delta(text: &str) -> ProgressEvent {
        ProgressEvent::StreamDelta { text: text.into() }
    }

    /// The text shown and the text spoken, frame by frame as a forwarder
    /// makes them.
    fn frames(markup: Markup, events: Vec<ProgressEvent>) -> Vec<(String, Option<String>)> {
        let mut parting = Parting::new(markup);
        let mut next = NextSpeech::default();
        let mut out = Vec::new();
        for event in events.into_iter().flat_map(|e| parting.event(e)) {
            match event {
                ProgressEvent::Speech { text } => next.set(text),
                ProgressEvent::StreamDelta { text } | ProgressEvent::Response { text } => {
                    out.push((text, next.take()));
                }
                _ => {}
            }
        }
        out
    }

    /// What a client concatenates: `speech ?? content`.
    fn heard(frames: &[(String, Option<String>)]) -> String {
        frames
            .iter()
            .map(|(shown, speech)| speech.as_deref().unwrap_or(shown))
            .collect()
    }

    fn shown(frames: &[(String, Option<String>)]) -> String {
        frames.iter().map(|(shown, _)| shown.as_str()).collect()
    }

    #[test]
    fn a_direction_split_across_deltas_reaches_only_the_voice() {
        let frames = frames(
            Markup::FishS1,
            vec![
                ProgressEvent::StreamStart,
                delta("(conf"),
                delta("ident) Found "),
                delta("it. Well (sigh"),
                delta("ing) yes."),
                ProgressEvent::StreamEnd,
            ],
        );
        assert_eq!(shown(&frames), "Found it. Well yes.");
        assert_eq!(heard(&frames), "(confident) Found it. Well (sighing) yes.");
        assert!(frames.iter().all(|(shown, _)| !shown.contains('(')));
    }

    /// A delta of nothing but a direction shows nothing and goes nowhere, but
    /// what it says isn't lost, at the end of the stream either.
    #[test]
    fn a_direction_alone_rides_with_what_follows_it() {
        let opening = frames(
            Markup::FishS1,
            vec![delta("(excited) "), delta("Yes"), ProgressEvent::StreamEnd],
        );
        assert_eq!(shown(&opening), "Yes");
        assert_eq!(heard(&opening), "(excited) Yes");
        let closing = frames(
            Markup::FishS1,
            vec![
                delta("Yes. "),
                delta("(laughing)"),
                ProgressEvent::StreamEnd,
            ],
        );
        assert_eq!(shown(&closing), "Yes. ");
        assert_eq!(heard(&closing), "Yes. (laughing)");
    }

    /// A chunk with nothing to part, and every chunk of a voice that takes no
    /// directions, is one frame as ever.
    #[test]
    fn speech_is_sent_only_where_it_differs() {
        let plain = frames(
            Markup::Plain,
            vec![
                delta("(excited) Found "),
                delta("it."),
                ProgressEvent::StreamEnd,
            ],
        );
        assert_eq!(shown(&plain), "Found it.");
        assert!(plain.iter().all(|(_, speech)| speech.is_none()));
        let fish = frames(
            Markup::FishS1,
            vec![
                delta("(excited) Found "),
                delta("it."),
                ProgressEvent::StreamEnd,
            ],
        );
        assert_eq!(heard(&fish), "(excited) Found it.");
        assert_eq!(fish[1], (" it.".to_string(), None));
    }

    #[test]
    fn a_retracted_stream_takes_its_speech_with_it() {
        let frames = frames(
            Markup::FishS1,
            vec![
                delta("(excited) Draft (exc"),
                ProgressEvent::StreamDiscard {
                    reason: StreamDiscardReason::Interrupted,
                },
                delta("(calm) Real."),
                ProgressEvent::StreamEnd,
            ],
        );
        assert_eq!(frames.last().unwrap().0, "Real.");
        assert_eq!(frames.last().unwrap().1.as_deref(), Some("(calm) Real."));
    }

    #[test]
    fn a_whole_answer_is_parted_too() {
        let frames = frames(
            Markup::FishS1,
            vec![ProgressEvent::Response {
                text: "(serious) (calm) Done.".into(),
            }],
        );
        assert_eq!(frames[0].0, "Done.");
        assert_eq!(frames[0].1.as_deref(), Some("(calm) Done."));
    }

    /// Screen and saved answer must stay word for word the same: a reply read
    /// back from history is the one that was on screen.
    #[test]
    fn what_streams_is_what_is_kept() {
        let answer = "(excited) Found it! (pensive) Hmm. It (sighing) broke. Ask Alice (API).";
        let chars: Vec<char> = answer.chars().collect();
        for cut in 0..=chars.len() {
            let (a, b): (String, String) =
                (chars[..cut].iter().collect(), chars[cut..].iter().collect());
            let frames = frames(
                Markup::FishS1,
                vec![delta(&a), delta(&b), ProgressEvent::StreamEnd],
            );
            assert_eq!(shown(&frames), Markup::Plain.apply(answer), "cut at {cut}");
        }
    }

    #[tokio::test]
    async fn the_relay_keeps_order_and_ends_with_its_source() {
        let (tx, rx) = mpsc::channel(8);
        let mut out = parted(rx, Markup::FishS1);
        tx.send(ProgressEvent::StreamStart).await.unwrap();
        tx.send(delta("(excited) Yes.")).await.unwrap();
        tx.send(ProgressEvent::StreamEnd).await.unwrap();
        drop(tx);
        assert!(matches!(out.recv().await, Some(ProgressEvent::StreamStart)));
        assert!(
            matches!(out.recv().await, Some(ProgressEvent::Speech { text }) if text == "(excited) Yes.")
        );
        assert!(
            matches!(out.recv().await, Some(ProgressEvent::StreamDelta { text }) if text == "Yes.")
        );
        assert!(matches!(out.recv().await, Some(ProgressEvent::StreamEnd)));
        assert!(out.recv().await.is_none());
    }

    fn session(metadata: serde_json::Value) -> Session {
        let mut session = Session::new("desk", "u", "a");
        session.metadata = metadata;
        session
    }

    #[test]
    fn a_spoken_turn_is_kept_without_its_directions() {
        let spoken =
            session(serde_json::json!({ "voice_turn": true, "tts_provider": "fish_audio" }));
        assert_eq!(spoken_with(&spoken), Some(Markup::FishS1));
        assert_eq!(
            kept_answer(&spoken, "(excited) Yes.").as_deref(),
            Some("Yes.")
        );
        let typed =
            session(serde_json::json!({ "voice_turn": false, "tts_provider": "fish_audio" }));
        assert_eq!(spoken_with(&typed), None);
        assert_eq!(kept_answer(&typed, "(excited) Yes."), None);
    }

    #[test]
    fn what_was_kept_with_directions_reads_without_them() {
        let fish = session(serde_json::json!({ "tts_provider": "fish_audio" }));
        assert_eq!(kept_text(&fish, "(confident) Yes (maybe)."), "Yes (maybe).");
        let other = session(serde_json::json!({ "tts_provider": "elevenlabs" }));
        assert_eq!(kept_text(&other, "(confident) Yes."), "(confident) Yes.");
        let typed = session(serde_json::json!({}));
        assert_eq!(kept_text(&typed, "(confident) Yes."), "(confident) Yes.");
    }

    /// The model reads its own past replies as the person read them, or it
    /// goes on to copy directions into typed answers.
    #[test]
    fn the_model_is_not_given_directions_it_once_wrote() {
        let mut spoken = session(serde_json::json!({ "tts_provider": "fish_audio" }));
        spoken.add_user_message("(excited) as I typed it", None);
        spoken.add_assistant_message("(confident) It works.", 3);
        clean_kept(&mut spoken);
        assert_eq!(spoken.messages[0].content, "(excited) as I typed it");
        assert_eq!(spoken.messages[1].content, "It works.");
    }
}
