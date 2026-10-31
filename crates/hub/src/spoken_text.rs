//! A spoken turn's reply on its way to the client that reads it aloud.
//!
//! The mobile app speaks with its own voice (Fish Audio on the device, or the
//! system's), and so does Desk with Kokoro: the hub never synthesises those
//! words, so the text it streams is the last place their voice directions
//! can be checked. Each spoken turn names its voice (see
//! [`workforce_agent::llm::speech::Markup`]); the directions that voice
//! performs reach the client, the rest never do.

use workforce_agent::llm::speech::{Markup, MarkupStream};

/// The spoken turn a stream belongs to: what its voice performs, and the
/// direction a piece of text may have left unfinished.
#[derive(Debug, Default)]
pub struct SpokenText {
    stream: Option<MarkupStream>,
    markup: Option<Markup>,
}

impl SpokenText {
    /// For a turn spoken with `markup`; `None` passes everything as it is.
    pub fn new(markup: Option<Markup>) -> Self {
        Self {
            stream: markup.map(Markup::stream),
            markup,
        }
    }

    /// The turn now streaming is spoken with `markup`: a different one
    /// starts afresh.
    pub fn follow(&mut self, markup: Option<Markup>) {
        if self.markup != markup {
            *self = Self::new(markup);
        }
    }

    /// A streamed piece, as far as it can be passed on yet.
    pub fn delta(&mut self, text: &str) -> String {
        match &mut self.stream {
            Some(stream) => stream.push(text),
            None => text.to_string(),
        }
    }

    /// The stream ended: what was held back.
    pub fn end(&mut self) -> String {
        self.stream
            .as_mut()
            .map(MarkupStream::finish)
            .unwrap_or_default()
    }

    /// The stream was retracted.
    pub fn discard(&mut self) {
        if let Some(stream) = &mut self.stream {
            stream.reset();
        }
    }

    /// A whole message.
    pub fn whole(&self, text: &str) -> String {
        match self.markup {
            Some(markup) => markup.apply(text),
            None => text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unspoken_turn_passes_through() {
        let mut text = SpokenText::new(None);
        assert_eq!(text.delta("(pensive) Hm"), "(pensive) Hm");
        assert_eq!(text.end(), "");
        assert_eq!(text.whole("(pensive) Hm"), "(pensive) Hm");
    }

    #[test]
    fn a_spoken_turn_keeps_only_what_its_voice_performs() {
        let mut text = SpokenText::new(Some(Markup::FishS1));
        let streamed = [text.delta("(excited) Yes! (pen"), text.delta("sive) Hm.")].concat();
        assert_eq!(streamed + &text.end(), "(excited) Yes! Hm.");
        assert_eq!(text.whole("(serious) Fine."), "Fine.");
    }

    #[test]
    fn another_voice_starts_afresh() {
        let mut text = SpokenText::new(Some(Markup::FishS1));
        assert_eq!(text.delta("Hi (exc"), "Hi");
        text.follow(Some(Markup::FishS1));
        // Mid-sentence, an S1 emotion is dropped.
        assert_eq!(text.delta("ited) there"), " there");
        text.follow(Some(Markup::Plain));
        assert_eq!(text.delta("(excited) New"), "New");
        text.follow(None);
        assert_eq!(text.delta("(excited) Typed"), "(excited) Typed");
    }

    #[test]
    fn a_discarded_stream_forgets_what_it_held() {
        let mut text = SpokenText::new(Some(Markup::Plain));
        assert_eq!(text.delta("Draft (exc"), "Draft");
        text.discard();
        assert_eq!(text.end(), "");
    }
}
