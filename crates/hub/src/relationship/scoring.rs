//! Pure, I/O-free scoring of a single human turn.
//!
//! Sentiment is sourced only from signals the user directs at the agent —
//! praise, gratitude and acceptance against correction, retry and stop. The
//! emotional charge of the *topic* under discussion is deliberately not a
//! signal: a user calmly working through a stressful problem still feels fine
//! about the agent, and conflating the two skewed the relationship negative.

use serde::Serialize;

/// How the user's message lands relative to the prior agent turn — the single,
/// user-sourced "did it help" axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    Praise,
    Gratitude,
    Acceptance,
    Correction,
    Retry,
    Stop,
    Neutral,
}

impl Reaction {
    pub fn as_str(self) -> &'static str {
        match self {
            Reaction::Praise => "praise",
            Reaction::Gratitude => "gratitude",
            Reaction::Acceptance => "acceptance",
            Reaction::Correction => "correction",
            Reaction::Retry => "retry",
            Reaction::Stop => "stop",
            Reaction::Neutral => "neutral",
        }
    }
}

/// Raw marker counts kept verbatim so the rolled-up summary stays interrogable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Signals {
    pub gratitude: u32,
    pub praise: u32,
    pub acceptance: u32,
    pub correction: u32,
    pub retry: u32,
    pub stop: u32,
    pub exclaim: u32,
    pub caps_words: u32,
    pub pos_emoji: u32,
    pub neg_emoji: u32,
}

/// The scored read of one turn.
#[derive(Debug, Clone)]
pub struct RelationshipRead {
    pub valence: f64,
    pub intensity: f64,
    pub reaction: Reaction,
    pub is_key_moment: bool,
    pub confidence: f64,
    pub signals: Signals,
}

impl RelationshipRead {
    /// A turn worth persisting: it carries a reaction, real valence, or a key
    /// moment. Flat, fully-neutral chatter is dropped so the log stays evidence
    /// — the scan cursor still advances past it.
    pub fn is_noteworthy(&self) -> bool {
        self.reaction != Reaction::Neutral
            || self.is_key_moment
            || self.valence.abs() >= NOTEWORTHY_VALENCE
    }
}

const GRATITUDE_WORDS: &[&str] = &[
    "thanks",
    "thank",
    "thx",
    "ty",
    "cheers",
    "appreciate",
    "appreciated",
    "grateful",
];
const GRATITUDE_PHRASES: &[&str] = &["thank you", "thanks a lot", "much appreciated"];
const PRAISE_WORDS: &[&str] = &[
    "perfect",
    "great",
    "brilliant",
    "awesome",
    "excellent",
    "amazing",
    "exactly",
    "wonderful",
    "fantastic",
    "superb",
];
const PRAISE_PHRASES: &[&str] = &[
    "love it",
    "well done",
    "good job",
    "spot on",
    "nailed it",
    "this is great",
];
const CORRECTION_WORDS: &[&str] = &["no", "nope", "wrong", "incorrect"];
const CORRECTION_PHRASES: &[&str] = &[
    "not what",
    "that's not",
    "thats not",
    "you misunderstood",
    "not right",
    "doesn't work",
    "does not work",
    "didn't work",
    "did not work",
    "not correct",
    "that is wrong",
    "instead of",
];
// The bare word "again" is deliberately absent: "run it again", "let's sync
// again", even "thanks again" are routine, not frustration. Retry intent comes
// from the unambiguous verbs and phrases below.
const RETRY_WORDS: &[&str] = &["retry", "redo"];
const RETRY_PHRASES: &[&str] = &["try again", "once more", "same problem", "still not"];
/// Friendly "no …" phrases that must not register as a correction — the bare
/// word "no" is a correction marker, but these are reassurance, not criticism.
const NO_BENIGN_PHRASES: &[&str] = &[
    "no problem",
    "no worries",
    "no rush",
    "no need",
    "no thanks",
];
/// Abandonment, not task control. The bare verbs "stop"/"cancel" are
/// deliberately absent: in operational use ("stop the polling", "cancel the
/// run") they are routine instructions, not the user giving up on the agent, so
/// matching them flooded the log with false-positive stop signals. Negated
/// encouragement ("don't give up") is filtered by [`STOP_BENIGN_PHRASES`].
const STOP_PHRASES: &[&str] = &["forget it", "never mind", "nevermind", "give up", "stop it"];
/// Negated forms that invert an abandonment phrase into encouragement of the
/// agent — the user cheering it on, not giving up. Mirrors [`NO_BENIGN_PHRASES`].
const STOP_BENIGN_PHRASES: &[&str] = &[
    "don't give up",
    "dont give up",
    "do not give up",
    "never give up",
];
const ACCEPTANCE_WORDS: &[&str] = &["ok", "okay", "yes", "yep", "yeah", "sure"];
const ACCEPTANCE_PHRASES: &[&str] = &[
    "got it",
    "sounds good",
    "will do",
    "makes sense",
    "that works",
    "looks good",
];

const POS_EMOJI: &[char] = &['🙏', '👍', '🎉', '😀', '😊', '✅', '🥳', '😄', '🙌', '❤'];
const NEG_EMOJI: &[char] = &['😞', '😡', '👎', '😤', '😠', '😢', '😩', '💢', '🙄'];

// A single negative marker reads as `negative`, not maximal: a routine "no,
// that's wrong" must not weigh the same as the user abandoning the agent. Only
// genuine abandonment (stop) or compounded/intense signals reach the strongest
// magnitudes.
const W_PRAISE: f64 = 0.45;
const W_GRATITUDE: f64 = 0.35;
const W_ACCEPT: f64 = 0.2;
const W_CORRECTION: f64 = -0.4;
const W_RETRY: f64 = -0.3;
const W_STOP: f64 = -0.55;
const W_POS_EMOJI: f64 = 0.2;
const W_NEG_EMOJI: f64 = -0.3;

const KEY_MOMENT_INTENSITY: f64 = 0.6;
const KEY_MOMENT_VALENCE: f64 = 0.5;
const NOTEWORTHY_VALENCE: f64 = 0.25;

fn tokens(text_lower: &str) -> Vec<&str> {
    text_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

fn word_hits(words: &[&str], targets: &[&str]) -> u32 {
    words.iter().filter(|w| targets.contains(w)).count() as u32
}

/// Count how many of `phrases` appear in the text, matched on whole-word
/// boundaries. Raw substring matching let a phrase fire as a sub-token of a
/// longer word ("give up" inside "give updates", "stop it" inside "stop
/// iterating", "will do" inside "will double"); requiring a non-alphanumeric
/// edge on both sides keeps the phrase a phrase. Counts each phrase at most
/// once, as the prior `contains` did.
fn phrase_hits(text_lower: &str, phrases: &[&str]) -> u32 {
    phrases
        .iter()
        .filter(|p| contains_phrase(text_lower, p))
        .count() as u32
}

/// Whether `phrase` occurs in `haystack` flanked by word boundaries (string
/// edge or a non-alphanumeric char). `phrase` is assumed lowercased.
fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    haystack.match_indices(phrase).any(|(start, m)| {
        let left_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let right_ok = haystack[start + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        left_ok && right_ok
    })
}

/// Fold the Unicode apostrophe variants phones and chat clients emit onto the
/// ASCII apostrophe the lexicon is written with, so a smart-quoted "don't" still
/// matches "don't". Only apostrophes are touched; everything else is preserved.
fn normalize_apostrophes(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{02BC}' | '\u{FF07}' => '\'',
            other => other,
        })
        .collect()
}

/// Score one user turn from its text alone.
pub fn score(user_text: &str) -> RelationshipRead {
    let lower = normalize_apostrophes(&user_text.to_lowercase());
    let words = tokens(&lower);

    let signals = Signals {
        gratitude: word_hits(&words, GRATITUDE_WORDS) + phrase_hits(&lower, GRATITUDE_PHRASES),
        praise: word_hits(&words, PRAISE_WORDS) + phrase_hits(&lower, PRAISE_PHRASES),
        acceptance: word_hits(&words, ACCEPTANCE_WORDS) + phrase_hits(&lower, ACCEPTANCE_PHRASES),
        correction: (word_hits(&words, CORRECTION_WORDS) + phrase_hits(&lower, CORRECTION_PHRASES))
            .saturating_sub(phrase_hits(&lower, NO_BENIGN_PHRASES)),
        retry: word_hits(&words, RETRY_WORDS) + phrase_hits(&lower, RETRY_PHRASES),
        stop: phrase_hits(&lower, STOP_PHRASES)
            .saturating_sub(phrase_hits(&lower, STOP_BENIGN_PHRASES)),
        exclaim: user_text.chars().filter(|&c| c == '!').count() as u32,
        caps_words: tokens(user_text)
            .iter()
            .filter(|w| w.chars().count() >= 3 && w.chars().all(|c| c.is_uppercase()))
            .count() as u32,
        pos_emoji: user_text.chars().filter(|c| POS_EMOJI.contains(c)).count() as u32,
        neg_emoji: user_text.chars().filter(|c| NEG_EMOJI.contains(c)).count() as u32,
    };

    let cap = |n: u32| n.min(2) as f64;
    let code_valence = (W_PRAISE * cap(signals.praise)
        + W_GRATITUDE * cap(signals.gratitude)
        + W_ACCEPT * cap(signals.acceptance)
        + W_CORRECTION * cap(signals.correction)
        + W_RETRY * cap(signals.retry)
        + W_STOP * cap(signals.stop)
        + W_POS_EMOJI * cap(signals.pos_emoji)
        + W_NEG_EMOJI * cap(signals.neg_emoji))
    .clamp(-1.0, 1.0);

    let markers = signals.gratitude
        + signals.praise
        + signals.acceptance
        + signals.correction
        + signals.retry
        + signals.stop;
    let intensity = (0.18 * markers as f64
        + 0.12 * signals.exclaim as f64
        + 0.15 * signals.caps_words as f64
        + 0.2 * (signals.pos_emoji + signals.neg_emoji) as f64)
        .clamp(0.0, 1.0);

    let reaction = classify_reaction(&signals);

    let confidence = if markers == 0 && signals.pos_emoji == 0 && signals.neg_emoji == 0 {
        0.2
    } else {
        (0.3 + 0.12 * markers as f64).min(0.9)
    };

    let is_key_moment =
        intensity >= KEY_MOMENT_INTENSITY && code_valence.abs() >= KEY_MOMENT_VALENCE;

    RelationshipRead {
        valence: code_valence,
        intensity,
        reaction,
        is_key_moment,
        confidence,
        signals,
    }
}

/// Negative reactions take priority over positive ones (a "thanks but no, that's
/// wrong" turn is a correction, not gratitude).
fn classify_reaction(s: &Signals) -> Reaction {
    if s.stop > 0 {
        Reaction::Stop
    } else if s.correction > 0 {
        Reaction::Correction
    } else if s.retry > 0 {
        Reaction::Retry
    } else if s.praise > 0 {
        Reaction::Praise
    } else if s.gratitude > 0 {
        Reaction::Gratitude
    } else if s.acceptance > 0 {
        Reaction::Acceptance
    } else {
        Reaction::Neutral
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> RelationshipRead {
        score(text)
    }

    #[test]
    fn gratitude_is_positive() {
        let r = read("perfect, thank you so much!");
        assert!(r.valence > 0.5, "valence {}", r.valence);
        assert_eq!(r.reaction, Reaction::Praise);
        assert!(r.is_noteworthy());
    }

    #[test]
    fn correction_is_negative_and_outranks_gratitude() {
        let r = read("thanks but no, that's not what I asked for");
        assert!(r.valence < 0.0, "valence {}", r.valence);
        assert_eq!(r.reaction, Reaction::Correction);
    }

    #[test]
    fn abandonment_phrase_is_stop_and_outranks_everything() {
        let r = read("thanks but forget it, I give up");
        assert_eq!(r.reaction, Reaction::Stop);
        assert!(r.valence < 0.0, "valence {}", r.valence);
    }

    #[test]
    fn operational_stop_and_cancel_are_not_frustration() {
        // Task-control verbs are not relationship signals: a day of "stop the
        // job / cancel the run" must not read as the user giving up on the agent.
        for t in [
            "stop",
            "cancel",
            "stop the polling",
            "stop the watch job",
            "cancel the run",
            "cancel that deploy",
        ] {
            let r = read(t);
            assert_eq!(r.signals.stop, 0, "{t:?} must not register a stop signal");
            assert_ne!(r.reaction, Reaction::Stop, "{t:?}");
        }
        // A polite operational stop keeps its positive read instead of flipping
        // to a negative Stop.
        let polite = read("ok, stop the job");
        assert_eq!(polite.reaction, Reaction::Acceptance);
        assert!(polite.valence > 0.0, "valence {}", polite.valence);
    }

    #[test]
    fn negated_give_up_is_encouragement_not_abandonment() {
        // "don't give up" is the user cheering the agent on — the opposite sign
        // of a real Stop, so it must not register one.
        for t in [
            "don't give up",
            "dont give up",
            "do not give up",
            "never give up, you've got this!",
        ] {
            let r = read(t);
            assert_eq!(r.signals.stop, 0, "{t:?} must not register a stop signal");
            assert_ne!(r.reaction, Reaction::Stop, "{t:?}");
        }
        // A genuine abandonment is unaffected by the guard.
        assert_eq!(read("forget it, I give up").reaction, Reaction::Stop);
    }

    #[test]
    fn phrase_matching_respects_word_boundaries() {
        // A signal phrase buried as a sub-token of a longer word must not fire,
        // across categories — this is the shared matcher, not stop-specific.
        assert_eq!(
            read("can you give updates on the deploy").signals.stop,
            0,
            "'give up' must not match inside 'give updates'"
        );
        assert_eq!(
            read("please stop iterating on the prototype").signals.stop,
            0,
            "'stop it' must not match inside 'stop iterating'"
        );
        assert_eq!(
            read("a workaround instead offering more delay")
                .signals
                .correction,
            0,
            "'instead of' must not match inside 'instead offering'"
        );
        assert_eq!(
            read("i will double-check the totals").signals.acceptance,
            0,
            "'will do' must not match inside 'will double'"
        );
        // The genuine, properly-bounded phrases still register.
        assert_eq!(read("ok i give up").signals.stop, 1);
        assert_eq!(read("do it instead of that").signals.correction, 1);
    }

    #[test]
    fn smart_quote_apostrophe_is_normalised() {
        // A curly apostrophe (U+2019, the default on phones and Slack) must not
        // slip past the negation guard.
        let r = read("don\u{2019}t give up");
        assert_eq!(r.signals.stop, 0, "curly-quote negation must be filtered");
        assert_ne!(r.reaction, Reaction::Stop);
        // And it must not break a positive apostrophe phrase either.
        assert!(read("that\u{2019}s not what I asked").signals.correction >= 1);
    }

    #[test]
    fn negation_substring_trap_is_avoided() {
        // "another" must not register as the word "no".
        let r = read("can you try another approach");
        assert_eq!(r.signals.correction, 0, "'another' must not match 'no'");
        assert_eq!(r.reaction, Reaction::Neutral);
    }

    #[test]
    fn benign_no_phrases_are_not_corrections() {
        for t in [
            "no problem!",
            "no worries, take your time",
            "no rush at all",
        ] {
            let r = read(t);
            assert_eq!(r.signals.correction, 0, "{t:?} must not be a correction");
            assert_ne!(r.reaction, Reaction::Correction, "{t:?}");
        }
        assert_eq!(read("no, that's wrong").reaction, Reaction::Correction);
    }

    #[test]
    fn acceptance_is_mildly_positive() {
        let r = read("ok, sounds good");
        assert_eq!(r.reaction, Reaction::Acceptance);
        assert!(r.valence > 0.0 && r.valence < 0.5);
    }

    #[test]
    fn neutral_question_is_low_confidence_and_not_noteworthy() {
        let r = read("what is the capital of France");
        assert_eq!(r.reaction, Reaction::Neutral);
        assert_eq!(r.confidence, 0.2);
        assert!(!r.is_noteworthy());
    }

    #[test]
    fn intensity_rises_with_caps_and_bangs() {
        let calm = read("that is wrong");
        let loud = read("that is WRONG!!!");
        assert!(loud.intensity > calm.intensity);
        assert!(loud.is_key_moment, "loud strong-negative is a key moment");
    }

    #[test]
    fn valence_is_clamped() {
        let r = read("perfect brilliant amazing excellent wonderful, thank you so much!!!");
        assert!(r.valence <= 1.0 && r.valence >= 0.99);
    }

    #[test]
    fn neutral_request_about_a_stressful_topic_stays_neutral() {
        // The topic's emotional charge is not a relationship signal: a friendly
        // request carries no user→agent valence, whatever it is about.
        for t in [
            "can you check the report",
            "the deploy is broken, can you take a look",
            "i'm worried about the deadline — can you help",
        ] {
            let r = read(t);
            assert_eq!(r.reaction, Reaction::Neutral, "{t:?}");
            assert_eq!(r.valence, 0.0, "{t:?} valence {}", r.valence);
            assert!(!r.is_noteworthy(), "{t:?}");
        }
    }

    #[test]
    fn again_is_not_a_retry_signal() {
        // "again" is routine, not frustration — it must not register a retry.
        for t in ["can you run it again", "let's sync again tomorrow"] {
            let r = read(t);
            assert_eq!(r.signals.retry, 0, "{t:?} must not register a retry");
            assert_eq!(r.reaction, Reaction::Neutral, "{t:?}");
        }
        // "thanks again" is pure gratitude, never a retry that cancels it out.
        let thanks = read("thanks again!");
        assert_eq!(thanks.reaction, Reaction::Gratitude);
        assert!(thanks.valence > 0.0, "valence {}", thanks.valence);
        // The unambiguous verbs and phrases still register.
        assert_eq!(read("please retry that").reaction, Reaction::Retry);
        assert_eq!(read("can you try again").reaction, Reaction::Retry);
    }

    #[test]
    fn a_single_correction_is_negative_not_maximal() {
        // One ordinary correction reads as negative but clear of the strongest
        // magnitudes, which are reserved for abandonment.
        let correction = read("that doesn't work");
        assert_eq!(correction.reaction, Reaction::Correction);
        assert!(
            correction.valence < 0.0 && correction.valence > -0.5,
            "single correction valence {}",
            correction.valence
        );
        // Giving up on the agent outweighs a mere correction.
        let stop = read("i give up");
        assert_eq!(stop.reaction, Reaction::Stop);
        assert!(
            stop.valence < correction.valence,
            "abandonment {} must be more negative than a correction {}",
            stop.valence,
            correction.valence
        );
    }
}
