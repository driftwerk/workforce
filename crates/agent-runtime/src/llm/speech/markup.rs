//! Voice directions in text that is going to be spoken: Fish Audio's
//! emotion, tone and sound tags, and what every other voice must never see.
//!
//! A speech model performs only the directions it was trained on and reads
//! anything else aloud as words, so one table here is both what the agent is
//! told it may use ([`Markup::guidance`]) and what is checked before any text
//! reaches a voice ([`Markup::apply`], and [`MarkupStream`] for text spoken
//! as it streams). The two cannot drift apart.
//!
//! Fish Audio's markers, as its docs give them (Emotion reference, Models
//! overview):
//!
//! - `s1` takes a fixed set in parentheses: 49 emotions, which open a
//!   sentence, and tone markers, sounds and pauses, which go anywhere.
//!   Anything else in parentheses is plain text.
//! - The S2 family (`s2-pro`, `s2.1-pro`, `s2.1-pro-free`) takes cues in
//!   square brackets, free-form; parentheses are plain text.
//!
//! What counts as a direction, so prose survives: a group in parentheses or
//! brackets of up to [`MAX_WORDS`] words of letters, spaces, hyphens and
//! apostrophes. One that names a known tag (or a near-miss of one, see
//! [`ALIASES`]) is a direction anywhere. Any other is one only when it is
//! all lower case and opens a sentence, or is a single word. `(see page 4)`,
//! `(API)` and `(a long aside, with commas)` are prose and are left alone.

/// How directions are written for the voice that will speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Markup {
    /// Fish Audio `s1`: the fixed set, in parentheses.
    FishS1,
    /// Fish Audio's S2 family: cues in square brackets.
    FishS2,
    /// Every other voice: no directions at all.
    Plain,
}

/// Where a tag may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// At the very start of a sentence.
    SentenceStart,
    /// Anywhere in the text.
    Anywhere,
}

struct Tag {
    name: &'static str,
    place: Place,
    /// Only the S2 family performs it.
    s2_only: bool,
}

const fn emotion(name: &'static str) -> Tag {
    Tag {
        name,
        place: Place::SentenceStart,
        s2_only: false,
    }
}

const fn anywhere(name: &'static str) -> Tag {
    Tag {
        name,
        place: Place::Anywhere,
        s2_only: false,
    }
}

const fn s2(name: &'static str) -> Tag {
    Tag {
        name,
        place: Place::Anywhere,
        s2_only: true,
    }
}

/// The 24 basic and 25 advanced emotions.
const EMOTIONS: &[Tag] = &[
    emotion("happy"),
    emotion("sad"),
    emotion("angry"),
    emotion("excited"),
    emotion("calm"),
    emotion("nervous"),
    emotion("confident"),
    emotion("surprised"),
    emotion("satisfied"),
    emotion("delighted"),
    emotion("scared"),
    emotion("worried"),
    emotion("upset"),
    emotion("frustrated"),
    emotion("depressed"),
    emotion("empathetic"),
    emotion("embarrassed"),
    emotion("disgusted"),
    emotion("moved"),
    emotion("proud"),
    emotion("relaxed"),
    emotion("grateful"),
    emotion("curious"),
    emotion("sarcastic"),
    emotion("disdainful"),
    emotion("unhappy"),
    emotion("anxious"),
    emotion("hysterical"),
    emotion("indifferent"),
    emotion("uncertain"),
    emotion("doubtful"),
    emotion("confused"),
    emotion("disappointed"),
    emotion("regretful"),
    emotion("guilty"),
    emotion("ashamed"),
    emotion("jealous"),
    emotion("envious"),
    emotion("hopeful"),
    emotion("optimistic"),
    emotion("pessimistic"),
    emotion("nostalgic"),
    emotion("lonely"),
    emotion("bored"),
    emotion("contemptuous"),
    emotion("sympathetic"),
    emotion("compassionate"),
    emotion("determined"),
    emotion("resigned"),
];

/// How it's said.
const TONES: &[Tag] = &[
    anywhere("in a hurry tone"),
    anywhere("shouting"),
    anywhere("screaming"),
    anywhere("whispering"),
    anywhere("soft tone"),
    s2("emphasis"),
];

/// Sounds the voice makes; the sound itself is best written after it.
const SOUNDS: &[Tag] = &[
    anywhere("laughing"),
    anywhere("chuckling"),
    anywhere("sobbing"),
    anywhere("crying loudly"),
    anywhere("sighing"),
    anywhere("groaning"),
    anywhere("panting"),
    anywhere("gasping"),
    anywhere("yawning"),
    anywhere("snoring"),
    s2("clear throat"),
    anywhere("audience laughing"),
    anywhere("background laughter"),
    anywhere("crowd laughing"),
];

const PAUSES: &[Tag] = &[anywhere("break"), anywhere("long-break")];

const TABLES: [&[Tag]; 4] = [EMOTIONS, TONES, SOUNDS, PAUSES];

/// Near-misses agents write, each as the tag it means. Inflections and true
/// synonyms only: a mood with no equivalent (`pensive`, `thoughtful`,
/// `serious`) is not guessed at, and is dropped.
pub const ALIASES: &[(&str, &str)] = &[
    ("laugh", "laughing"),
    ("laughs", "laughing"),
    ("laughter", "laughing"),
    ("chuckle", "chuckling"),
    ("chuckles", "chuckling"),
    ("giggle", "chuckling"),
    ("giggles", "chuckling"),
    ("sigh", "sighing"),
    ("sighs", "sighing"),
    ("gasp", "gasping"),
    ("gasps", "gasping"),
    ("groan", "groaning"),
    ("groans", "groaning"),
    ("yawn", "yawning"),
    ("yawns", "yawning"),
    ("sob", "sobbing"),
    ("sobs", "sobbing"),
    ("pant", "panting"),
    ("pants", "panting"),
    ("whisper", "whispering"),
    ("whispers", "whispering"),
    ("whispered", "whispering"),
    ("shout", "shouting"),
    ("shouts", "shouting"),
    ("scream", "screaming"),
    ("screams", "screaming"),
    ("soft", "soft tone"),
    ("softly", "soft tone"),
    ("gently", "soft tone"),
    ("quietly", "soft tone"),
    ("hurried", "in a hurry tone"),
    ("hurriedly", "in a hurry tone"),
    ("in a hurry", "in a hurry tone"),
    ("pause", "break"),
    ("short pause", "break"),
    ("long pause", "long-break"),
    ("long break", "long-break"),
    ("clears throat", "clear throat"),
    ("happily", "happy"),
    ("cheerful", "happy"),
    ("cheerfully", "happy"),
    ("joyful", "happy"),
    ("glad", "happy"),
    ("sadly", "sad"),
    ("angrily", "angry"),
    ("excitedly", "excited"),
    ("enthusiastic", "excited"),
    ("thrilled", "excited"),
    ("calmly", "calm"),
    ("nervously", "nervous"),
    ("confidently", "confident"),
    ("shocked", "surprised"),
    ("amused", "delighted"),
    ("concerned", "worried"),
    ("worriedly", "worried"),
    ("anxiously", "anxious"),
    ("annoyed", "frustrated"),
    ("irritated", "frustrated"),
    ("afraid", "scared"),
    ("fearful", "scared"),
    ("frightened", "scared"),
    ("puzzled", "confused"),
    ("hesitant", "uncertain"),
    ("unsure", "uncertain"),
    ("skeptical", "doubtful"),
    ("sceptical", "doubtful"),
    ("apologetic", "regretful"),
    ("thankful", "grateful"),
    ("sarcastically", "sarcastic"),
    ("curiously", "curious"),
    ("intrigued", "curious"),
    ("proudly", "proud"),
    ("sympathetically", "sympathetic"),
    ("resolute", "determined"),
];

/// The most words a direction has.
pub const MAX_WORDS: usize = 6;
/// The longest a direction is, brackets included.
const MAX_CHARS: usize = 48;

impl Markup {
    /// The markup of `provider` (any spelling [`super::canonical_speech_provider`]
    /// takes) speaking with `model`. Fish without a model is `s1`, the
    /// hub's and the mobile app's default. A Fish model this table doesn't
    /// know is [`Markup::Plain`]: stripping a direction it would have
    /// performed costs some expression, keeping one it would read costs the
    /// listener a word in brackets.
    pub fn of(provider: &str, model: Option<&str>) -> Self {
        if super::canonical_speech_provider(provider) != Some("fish_audio") {
            return Self::Plain;
        }
        let model = model.map(|m| m.trim().to_ascii_lowercase());
        match model.as_deref() {
            None | Some("") | Some("s1") => Self::FishS1,
            Some(m) if m.starts_with("s2") => Self::FishS2,
            Some(_) => Self::Plain,
        }
    }

    /// `text` with only the directions this voice performs, each written as
    /// it takes them.
    pub fn apply(self, text: &str) -> String {
        let mut cursor = Cursor::default();
        let mut out = String::with_capacity(text.len());
        rewrite(self, text, &mut cursor, &mut out);
        out
    }

    /// A filter for text spoken as it streams in.
    pub fn stream(self) -> MarkupStream {
        MarkupStream {
            markup: self,
            held: String::new(),
            cursor: Cursor::default(),
        }
    }

    /// What the agent is told about directions for this voice; `None` for a
    /// voice that takes none.
    pub fn guidance(self) -> Option<String> {
        let list = |tags: &[Tag], open: char, close: char| {
            tags.iter()
                .filter(|t| self == Self::FishS2 || !t.s2_only)
                .map(|t| format!("{open}{}{close}", t.name))
                .collect::<Vec<_>>()
                .join(" ")
        };
        match self {
            Self::Plain => None,
            Self::FishS1 => Some(format!(
                "\
TTS provider: Fish Audio (S1)
- You can direct the voice with tags in parentheses. Only the tags below are
  performed: any other word in parentheses would be read out loud, so it is
  removed before you are heard. Never invent one.
- Emotions open a sentence, one or two at most:
  {emotions}
- Tone, anywhere in a sentence: {tones}
- Sounds, anywhere; write the sound after it, as in \"(laughing) Ha, ha\":
  {sounds}
- Pauses: {pauses}
- Use them where a person would really sound that way, not on every
  sentence: \"(excited) Found it!\" or \"(soft tone) That one's on me.\"",
                emotions = list(EMOTIONS, '(', ')'),
                tones = list(TONES, '(', ')'),
                sounds = list(SOUNDS, '(', ')'),
                pauses = list(PAUSES, '(', ')'),
            )),
            Self::FishS2 => Some(format!(
                "\
TTS provider: Fish Audio (S2)
- You can direct the voice with cues in square brackets, never parentheses
  (those are read out loud).
- An emotion goes at the start of the sentence it colours:
  {emotions}
- Tone, anywhere: {tones}
- Sounds, anywhere; write the sound after it, as in \"[laughing] Ha, ha\":
  {sounds}
- Pauses: {pauses}
- A short description works too, like [warmly] or [tired], kept to a few words.
- Use them where a person would really sound that way, not on every sentence.",
                emotions = list(EMOTIONS, '[', ']'),
                tones = list(TONES, '[', ']'),
                sounds = list(SOUNDS, '[', ']'),
                pauses = list(PAUSES, '[', ']'),
            )),
        }
    }
}

/// [`Markup::apply`] for text that arrives in pieces: a direction split
/// across two pieces is held back until it's whole.
#[derive(Debug, Clone)]
pub struct MarkupStream {
    markup: Markup,
    held: String,
    cursor: Cursor,
}

impl MarkupStream {
    /// What of everything so far can be passed on.
    pub fn push(&mut self, piece: &str) -> String {
        self.held.push_str(piece);
        let keep = open_tail(&self.held);
        let ready: String = self.held.drain(..keep).collect();
        let mut out = String::with_capacity(ready.len());
        rewrite(self.markup, &ready, &mut self.cursor, &mut out);
        out
    }

    /// The end of the text: whatever was held back, as it stands.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.held);
        let mut out = String::with_capacity(rest.len());
        rewrite(self.markup, &rest, &mut self.cursor, &mut out);
        self.cursor = Cursor::default();
        out
    }

    /// Starts again, dropping what was held: the text was retracted.
    pub fn reset(&mut self) {
        self.held.clear();
        self.cursor = Cursor::default();
    }
}

/// Where in the text the rewrite has got to.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    /// The next character starts a sentence.
    sentence_start: bool,
    /// The last character written, if any.
    last: Option<char>,
    /// A direction was just dropped where a space already stood: the space
    /// after it goes too.
    skip_space: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            sentence_start: true,
            last: None,
            skip_space: false,
        }
    }
}

impl Cursor {
    fn wrote(&mut self, c: char) {
        self.last = Some(c);
        if matches!(c, '.' | '!' | '?' | '…' | ':' | '\n') {
            self.sentence_start = true;
        } else if !c.is_whitespace() && !matches!(c, '"' | '“' | '\'' | '‘') {
            self.sentence_start = false;
        }
    }

    fn after_space(&self) -> bool {
        self.last.is_none_or(char::is_whitespace)
    }
}

fn closer(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        _ => None,
    }
}

fn direction_char(c: char) -> bool {
    c.is_alphabetic() || matches!(c, ' ' | '-' | '\'' | '’')
}

/// Where the trailing part of `text` that the next piece could still change
/// begins: an opened group with nothing yet ruling out a direction, or a
/// direction it ends with (what follows decides the spacing), and the spaces
/// before either. `text.len()` when there is none.
fn open_tail(text: &str) -> usize {
    let group = text.rfind(['(', '[']).filter(|&at| {
        let open = text[at..].chars().next().and_then(closer);
        let rest = &text[at + 1..];
        let unfinished = 1 + rest.len() < MAX_CHARS
            && open.is_some_and(|close| !rest.contains(close))
            && rest.chars().all(direction_char);
        unfinished || group(&text[at..]).is_some_and(|(_, len)| at + len == text.len())
    });
    let end = group.unwrap_or(text.len());
    text[..end].trim_end().len()
}

/// The group opening `text`, if it has the shape of a direction: its words,
/// and how many bytes it spans.
fn group(text: &str) -> Option<(&str, usize)> {
    let mut chars = text.char_indices();
    let (_, open) = chars.next()?;
    let close = closer(open)?;
    for (i, c) in chars {
        if i >= MAX_CHARS {
            return None;
        }
        if c == close {
            let inner = &text[1..i];
            let words = inner.split_whitespace().count();
            return (words > 0 && words <= MAX_WORDS).then_some((inner, i + c.len_utf8()));
        }
        if !direction_char(c) {
            return None;
        }
    }
    None
}

fn words(inner: &str) -> String {
    inner
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('’', "'")
        .to_lowercase()
}

/// The tag `words` names, directly or as a near-miss, in either family.
fn tag(words: &str) -> Option<&'static Tag> {
    let name = ALIASES
        .iter()
        .find(|(alias, _)| *alias == words)
        .map_or(words, |(_, tag)| *tag);
    TABLES
        .iter()
        .flat_map(|t| t.iter())
        .find(|t| t.name == name)
}

/// What becomes of one direction-shaped group.
enum Verdict {
    /// Not a direction: prose, left as it is.
    Prose,
    Drop,
    Write(String),
}

fn judge(markup: Markup, inner: &str, sentence_start: bool) -> Verdict {
    let said = words(inner);
    // Any family's tag is a direction, so a voice that can't perform it
    // still has it removed.
    let known = tag(&said);
    let lower = !inner.chars().any(char::is_uppercase);
    let single = said.split(' ').count() == 1;
    if known.is_none() && !(lower && (sentence_start || single)) {
        return Verdict::Prose;
    }
    match (markup, known) {
        (Markup::Plain, _) => Verdict::Drop,
        (Markup::FishS1, Some(t))
            if !t.s2_only && (t.place == Place::Anywhere || sentence_start) =>
        {
            Verdict::Write(format!("({})", t.name))
        }
        (Markup::FishS1, _) => Verdict::Drop,
        (Markup::FishS2, Some(t)) => Verdict::Write(format!("[{}]", t.name)),
        (Markup::FishS2, None) => Verdict::Write(format!("[{said}]")),
    }
}

fn rewrite(markup: Markup, text: &str, cursor: &mut Cursor, out: &mut String) {
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if std::mem::take(&mut cursor.skip_space) && c == ' ' {
            i += 1;
            continue;
        }
        // Attached to a word it's a call or a plural, `f(x)` or `file(s)`.
        let attached = cursor.last.is_some_and(char::is_alphanumeric);
        if let Some((inner, len)) = group(&text[i..]).filter(|_| !attached) {
            match judge(markup, inner, cursor.sentence_start) {
                Verdict::Prose => {}
                Verdict::Drop => {
                    let next = text[i + len..].chars().next();
                    if out.ends_with(' ') && next.is_some_and(|n| ".,!?;:".contains(n)) {
                        out.pop();
                        cursor.last = out.chars().last().or(Some(' '));
                    } else {
                        cursor.skip_space = cursor.after_space();
                    }
                    i += len;
                    continue;
                }
                Verdict::Write(direction) => {
                    let start = cursor.sentence_start;
                    out.push_str(&direction);
                    cursor.last = direction.chars().last();
                    // Directions chain: "(sad)(soft tone) Oh."
                    cursor.sentence_start = start;
                    i += len;
                    continue;
                }
            }
        }
        out.push(c);
        cursor.wrote(c);
        i += c.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_picks_the_markup() {
        assert_eq!(Markup::of("fish_audio", None), Markup::FishS1);
        assert_eq!(Markup::of("fishaudio", Some("S1")), Markup::FishS1);
        assert_eq!(Markup::of("fish_audio", Some(" ")), Markup::FishS1);
        assert_eq!(Markup::of("fish_audio", Some("s2-pro")), Markup::FishS2);
        assert_eq!(Markup::of("fish", Some("s2.1-pro-free")), Markup::FishS2);
        assert_eq!(Markup::of("fish_audio", Some("s3")), Markup::Plain);
        for other in [
            "elevenlabs",
            "openai",
            "builtin",
            "soniox",
            "kokoro",
            "system",
            "",
        ] {
            assert_eq!(Markup::of(other, Some("s1")), Markup::Plain, "{other}");
        }
    }

    #[test]
    fn s1_keeps_its_tags_and_drops_the_rest() {
        let s1 = Markup::FishS1;
        assert_eq!(s1.apply("(excited) Found it!"), "(excited) Found it!");
        assert_eq!(s1.apply("(pensive) Hmm, tricky."), "Hmm, tricky.");
        assert_eq!(s1.apply("(thoughtful) Let me see."), "Let me see.");
        assert_eq!(
            s1.apply("Done. (serious) Check the logs."),
            "Done. Check the logs."
        );
        assert_eq!(
            s1.apply("(sad)(soft tone) Oh no."),
            "(sad)(soft tone) Oh no."
        );
    }

    #[test]
    fn near_misses_become_the_tag_they_mean() {
        let s1 = Markup::FishS1;
        assert_eq!(s1.apply("(Laughs) Ha, ha."), "(laughing) Ha, ha.");
        assert_eq!(s1.apply("(concerned) That's odd."), "(worried) That's odd.");
        assert_eq!(s1.apply("(amused) Nice."), "(delighted) Nice.");
        assert_eq!(s1.apply("Wait (long pause) yes."), "Wait (long-break) yes.");
        assert_eq!(s1.apply("[excited] Yes!"), "(excited) Yes!");
    }

    #[test]
    fn an_emotion_only_opens_a_sentence_on_s1() {
        let s1 = Markup::FishS1;
        assert_eq!(s1.apply("It broke (sad) again."), "It broke again.");
        assert_eq!(
            s1.apply("It broke (sighing) again."),
            "It broke (sighing) again."
        );
        assert_eq!(s1.apply("Right.\n(calm) Next."), "Right.\n(calm) Next.");
        assert_eq!(
            s1.apply("“(happy) Hi,” she said."),
            "“(happy) Hi,” she said."
        );
    }

    #[test]
    fn s2_tags_are_s2_only() {
        assert_eq!(Markup::FishS1.apply("Ahem (clear throat) so."), "Ahem so.");
        assert_eq!(
            Markup::FishS2.apply("Ahem (clear throat) so."),
            "Ahem [clear throat] so."
        );
        assert_eq!(Markup::FishS1.apply("(emphasis) Now."), "Now.");
    }

    #[test]
    fn s2_writes_every_direction_in_brackets() {
        let s2 = Markup::FishS2;
        assert_eq!(s2.apply("(excited) Found it!"), "[excited] Found it!");
        assert_eq!(s2.apply("(pensive) Hmm."), "[pensive] Hmm.");
        assert_eq!(s2.apply("[warmly] Welcome back."), "[warmly] Welcome back.");
        assert_eq!(s2.apply("(laughs) Ha."), "[laughing] Ha.");
    }

    #[test]
    fn other_voices_get_no_directions() {
        let plain = Markup::Plain;
        assert_eq!(plain.apply("(excited) Found it!"), "Found it!");
        assert_eq!(plain.apply("[excited] Found it!"), "Found it!");
        assert_eq!(plain.apply("Well (laughing) fine."), "Well fine.");
        assert_eq!(plain.apply("Ahem (clear throat) so."), "Ahem so.");
        assert_eq!(plain.apply("(pensive) Hmm."), "Hmm.");
    }

    #[test]
    fn prose_in_brackets_is_left_alone() {
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            for prose in [
                "Ask your manager (Alice) first.",
                "The API (v1.2) is out.",
                "See the docs (page 4).",
                "It ran (all three suites, in fact) and passed.",
                "Two things (both small) are left.",
                "It returned [1, 2, 3].",
                "(This matters.) Read it.",
                "Use f(x) here.",
                "Unclosed (bracket here",
            ] {
                assert_eq!(markup.apply(prose), prose, "{markup:?}: {prose}");
            }
        }
    }

    #[test]
    fn a_lower_case_word_alone_in_brackets_is_a_direction() {
        assert_eq!(
            Markup::Plain.apply("Pass the flag (optional) too."),
            "Pass the flag too."
        );
        assert_eq!(
            Markup::FishS1.apply("Pass the flag (optional) too."),
            "Pass the flag too."
        );
    }

    #[test]
    fn spacing_is_tidied_where_a_direction_was() {
        assert_eq!(Markup::Plain.apply("  (excited) Hi."), "  Hi.");
        assert_eq!(Markup::Plain.apply("Hi. (sad) Bye."), "Hi. Bye.");
        assert_eq!(Markup::Plain.apply("Hi.(sad) Bye."), "Hi. Bye.");
        assert_eq!(Markup::Plain.apply("Hi (sighing). Bye."), "Hi. Bye.");
        assert_eq!(Markup::Plain.apply("Hi (sighing), you."), "Hi, you.");
    }

    fn streamed(markup: Markup, pieces: &[&str]) -> String {
        let mut stream = markup.stream();
        let mut out: String = pieces.iter().map(|p| stream.push(p)).collect();
        out.push_str(&stream.finish());
        out
    }

    #[test]
    fn a_stream_says_what_the_whole_text_would() {
        let text = "(excited) Found it! (pensive) Hmm. It broke (sad) again (sighing) twice. \
                    Ask Alice (API) or [warmly] me. (Laughs) Ha. Unclosed (end";
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            let whole = markup.apply(text);
            let chars: Vec<char> = text.chars().collect();
            for cut in 0..=chars.len() {
                let (a, b): (String, String) =
                    (chars[..cut].iter().collect(), chars[cut..].iter().collect());
                assert_eq!(
                    streamed(markup, &[&a, &b]),
                    whole,
                    "{markup:?} cut at {cut}"
                );
            }
            let singles: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
            let refs: Vec<&str> = singles.iter().map(String::as_str).collect();
            assert_eq!(streamed(markup, &refs), whole, "{markup:?} char by char");
        }
    }

    #[test]
    fn a_stream_holds_back_only_what_might_be_a_direction() {
        let mut stream = Markup::Plain.stream();
        assert_eq!(stream.push("Hello (exc"), "Hello");
        assert_eq!(stream.push("ited) there"), " there");
        assert_eq!(stream.push(" and f(x, y"), " and f(x, y");
        assert_eq!(stream.push(" is "), " is");
        assert_eq!(stream.finish(), " ");
    }

    #[test]
    fn a_reset_stream_drops_what_it_held() {
        let mut stream = Markup::FishS1.stream();
        assert_eq!(stream.push("Okay. (exc"), "Okay.");
        stream.reset();
        assert_eq!(stream.push("(excited) New."), "(excited) New.");
    }

    #[test]
    fn guidance_lists_exactly_what_the_model_performs() {
        let s1 = Markup::FishS1.guidance().unwrap();
        for t in TABLES.iter().flat_map(|t| t.iter()) {
            assert_eq!(
                s1.contains(&format!("({})", t.name)),
                !t.s2_only,
                "s1 guidance and {}",
                t.name
            );
        }
        for stale in ["thoughtful", "concerned", "amused", "serious", "pensive"] {
            assert!(!s1.contains(stale), "s1 guidance offers {stale}");
        }
        let s2 = Markup::FishS2.guidance().unwrap();
        assert!(s2.contains("[clear throat]") && s2.contains("[emphasis]"));
        assert!(!s2.contains("(excited)"));
        assert!(Markup::Plain.guidance().is_none());
    }

    #[test]
    fn every_alias_names_a_real_tag() {
        for (alias, target) in ALIASES {
            assert!(tag(target).is_some(), "{alias} → {target}");
            assert!(tag(alias).is_some(), "{alias}");
        }
    }

    #[test]
    fn every_s1_tag_survives_s1() {
        for t in TABLES.iter().flat_map(|t| t.iter()).filter(|t| !t.s2_only) {
            let text = format!("({}) Yes.", t.name);
            assert_eq!(Markup::FishS1.apply(&text), text);
        }
    }
}
