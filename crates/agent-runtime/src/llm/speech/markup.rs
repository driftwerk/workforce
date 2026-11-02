//! Voice directions in text that is going to be spoken: Fish Audio's
//! emotion, tone and sound tags, and what every other voice must never see.
//!
//! A speech model performs only the directions it was trained on and reads
//! anything else aloud as words, so one table here is both what the agent is
//! told it may use ([`Markup::guidance`]) and what is checked before any text
//! reaches a voice ([`Markup::apply`]). The two cannot drift apart.
//!
//! Directions are for the voice alone. What a reader sees, and what is kept,
//! has none: [`Markup::split`] parts a reply as it streams into the text to
//! show and the text to speak.
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
//! apostrophes, outside code and link text. One that names a known tag (or a
//! near-miss of one, see [`ALIASES`]) is a direction anywhere. `(see page 4)`,
//! `(API)` and `(a long aside, with commas)` are prose and are left alone.
//!
//! The two sides guess differently, because a wrong guess costs differently.
//! A word read out in brackets costs the listener a moment, so what's spoken
//! also loses any other group that is all lower case and opens a sentence or
//! is a single word of two letters or more. What's read and saved loses content
//! for good, so it loses only what names a direction ([`Markup::shown`]).

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

/// Moods agents write that no voice performs and nothing here means: never
/// offered, but named, so that they don't reach the screen.
const UNPERFORMED: &[&str] = &["pensive", "thoughtful", "serious"];

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

    /// `text` as a reader is to see it and as it's kept: without the directions
    /// of a reply spoken in this voice.
    pub fn shown(self, text: &str) -> String {
        let mut cursor = Cursor::default();
        let mut out = String::with_capacity(text.len());
        rewrite_shown(self, text, &mut cursor, &mut out);
        out
    }

    /// Parts a reply as it streams in: the text to show, and the text to
    /// speak in this voice.
    pub fn split(self) -> MarkupSplit {
        MarkupSplit {
            markup: self,
            held: String::new(),
            shown: Cursor::default(),
            spoken: Cursor::default(),
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
- A short description works too, like [warmly] or [tired], kept to a few words,
  at the start of a sentence.
- Use them where a person would really sound that way, not on every sentence.",
                emotions = list(EMOTIONS, '[', ']'),
                tones = list(TONES, '[', ']'),
                sounds = list(SOUNDS, '[', ']'),
                pauses = list(PAUSES, '[', ']'),
            )),
        }
    }
}

/// `text` without what names a direction: how a message saved before directions
/// were parted from the text reads now. It may hold what was never meant as one,
/// so a word that's ordinary out of place stays there: an emotion, a pause or a
/// near-miss has to open a sentence, as in `(happy) Yes`, not `the (pause) button`.
/// A sound or a tone, by its own name, goes anywhere.
pub fn without_named_directions(text: &str) -> String {
    let mut cursor = Cursor::default();
    let mut out = String::with_capacity(text.len());
    rewrite_with(
        |inner, sentence_start, _| {
            let said = words(inner);
            let anywhere = [TONES, SOUNDS]
                .iter()
                .flat_map(|t| t.iter())
                .any(|t| t.name == said);
            if anywhere || (sentence_start && named(&said)) {
                Verdict::Drop
            } else {
                Verdict::Prose
            }
        },
        text,
        &mut cursor,
        &mut out,
    );
    out
}

/// One piece of a reply, parted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parted {
    /// What a reader sees: no directions.
    pub shown: String,
    /// What the voice is given: the directions it performs.
    pub spoken: String,
}

/// [`Markup::split`]: a direction split across two pieces is held back until
/// it's whole. What is held doesn't depend on the voice, so the two texts
/// advance together, piece for piece.
#[derive(Debug, Clone)]
pub struct MarkupSplit {
    markup: Markup,
    held: String,
    shown: Cursor,
    spoken: Cursor,
}

impl MarkupSplit {
    /// What of everything so far can be passed on.
    pub fn push(&mut self, piece: &str) -> Parted {
        self.held.push_str(piece);
        let keep = open_tail(&self.held);
        let ready: String = self.held.drain(..keep).collect();
        self.part(&ready)
    }

    /// The end of the text: whatever was held back, as it stands.
    pub fn finish(&mut self) -> Parted {
        let rest = std::mem::take(&mut self.held);
        let parted = self.part(&rest);
        self.shown = Cursor::default();
        self.spoken = Cursor::default();
        parted
    }

    /// Starts again, dropping what was held: the text was retracted.
    pub fn reset(&mut self) {
        self.held.clear();
        self.shown = Cursor::default();
        self.spoken = Cursor::default();
    }

    fn part(&mut self, text: &str) -> Parted {
        let mut parted = Parted::default();
        rewrite_shown(self.markup, text, &mut self.shown, &mut parted.shown);
        rewrite(self.markup, text, &mut self.spoken, &mut parted.spoken);
        parted
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
    /// The last thing written was a direction.
    directed: bool,
    /// Only spaces so far on this line.
    line_start: bool,
    code: Code,
}

/// Code the rewrite is inside, by the length of the backtick run that opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Code {
    Outside,
    /// Closed by a run of the same length. Ends with its line at the latest: a
    /// stray backtick mustn't protect the rest of a reply.
    Inline(usize),
    /// Closed by a run at least as long, opening a line.
    Fence(usize),
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            sentence_start: true,
            last: None,
            skip_space: false,
            directed: false,
            line_start: true,
            code: Code::Outside,
        }
    }
}

impl Cursor {
    fn wrote(&mut self, c: char) {
        self.last = Some(c);
        self.directed = false;
        if c == '\n' {
            self.line_start = true;
            if matches!(self.code, Code::Inline(_)) {
                self.code = Code::Outside;
            }
        } else if !matches!(c, ' ' | '\t') {
            self.line_start = false;
        }
        if matches!(c, '.' | '!' | '?' | '…' | ':' | '\n') {
            self.sentence_start = true;
        } else if !c.is_whitespace() && !matches!(c, '"' | '“' | '\'' | '‘') {
            self.sentence_start = false;
        }
    }

    fn after_space(&self) -> bool {
        self.last.is_none_or(char::is_whitespace)
    }

    /// A run of `ticks` backticks comes next.
    fn backticks(&mut self, ticks: usize) {
        self.code = match self.code {
            Code::Outside if self.line_start && ticks >= 3 => Code::Fence(ticks),
            Code::Outside => Code::Inline(ticks),
            Code::Inline(opened) if ticks == opened => Code::Outside,
            Code::Fence(opened) if self.line_start && ticks >= opened => Code::Outside,
            code => code,
        };
    }

    /// `open` follows a closing bracket, so it's a link's `[text](target)` or
    /// `[text][label]`. A direction written just before doesn't count: `[sad][calm]`.
    fn continues_link(&self, open: char) -> bool {
        matches!(open, '(' | '[') && self.last == Some(']') && !self.directed
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
/// direction it ends with (what follows decides the spacing), a bracket that
/// a group opening after it would make a link's text, a run of backticks (what's
/// next may lengthen it, and code depends on how long it is), and the spaces
/// before any of them. `text.len()` when there is none.
fn open_tail(text: &str) -> usize {
    let group = text.rfind(['(', '[']).filter(|&at| {
        let open = text[at..].chars().next().and_then(closer);
        let rest = &text[at + 1..];
        let unfinished = 1 + rest.len() < MAX_CHARS
            && open.is_some_and(|close| !rest.contains(close))
            && rest.chars().all(direction_char);
        unfinished || group(&text[at..]).is_some_and(|(_, len)| at + len == text.len())
    });
    let mut end = group.unwrap_or(text.len());
    if text[end..].starts_with(['(', '[']) && text[..end].ends_with(']') {
        let bracket = text[..end].rfind('[');
        if let Some(at) =
            bracket.filter(|&at| self::group(&text[at..]).is_some_and(|(_, len)| at + len == end))
        {
            end = at;
        }
    }
    let end = end.min(text.trim_end_matches('`').len());
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

/// Whether `words` names a direction, whether or not any voice performs it.
fn named(words: &str) -> bool {
    tag(words).is_some() || UNPERFORMED.contains(&words)
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
    // A letter alone is a list's `(a)` or a task's `[x]`.
    let letter = said.chars().count() == 1;
    if known.is_none() && (letter || !(lower && (sentence_start || single))) {
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
    rewrite_with(
        |inner, sentence_start, _| judge(markup, inner, sentence_start),
        text,
        cursor,
        out,
    );
}

/// What a reader keeps of a direction-shaped group: all but what names a
/// direction, or, for a voice told it may describe its own at the start of a
/// sentence, one of those.
fn rewrite_shown(markup: Markup, text: &str, cursor: &mut Cursor, out: &mut String) {
    rewrite_with(
        |inner, sentence_start, open| {
            let said = words(inner);
            let described = markup == Markup::FishS2
                && open == '['
                && sentence_start
                && said.chars().count() > 1
                && !inner.chars().any(char::is_uppercase);
            if named(&said) || described {
                Verdict::Drop
            } else {
                Verdict::Prose
            }
        },
        text,
        cursor,
        out,
    );
}

fn rewrite_with(
    judge: impl Fn(&str, bool, char) -> Verdict,
    text: &str,
    cursor: &mut Cursor,
    out: &mut String,
) {
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if std::mem::take(&mut cursor.skip_space) && c == ' ' {
            i += 1;
            continue;
        }
        if c == '`' {
            let ticks = text[i..].chars().take_while(|c| *c == '`').count();
            cursor.backticks(ticks);
            out.push_str(&text[i..i + ticks]);
            cursor.wrote('`');
            i += ticks;
            continue;
        }
        // Attached to a word it's a call or a plural, `f(x)` or `file(s)`.
        let attached = cursor.last.is_some_and(char::is_alphanumeric) || cursor.continues_link(c);
        let code = cursor.code != Code::Outside;
        if let Some((inner, len)) = group(&text[i..]).filter(|_| !attached && !code) {
            let next = text[i + len..].chars().next();
            let link =
                c == '[' && (next == Some('(') || (next == Some('[') && !named(&words(inner))));
            let verdict = if link {
                Verdict::Prose
            } else {
                judge(inner, cursor.sentence_start, c)
            };
            match verdict {
                Verdict::Prose => {}
                Verdict::Drop => {
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
                    cursor.directed = true;
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

    fn parted(markup: Markup, pieces: &[&str]) -> Parted {
        let mut split = markup.split();
        let mut out = Parted::default();
        for piece in pieces {
            let next = split.push(piece);
            out.shown.push_str(&next.shown);
            out.spoken.push_str(&next.spoken);
        }
        let last = split.finish();
        out.shown.push_str(&last.shown);
        out.spoken.push_str(&last.spoken);
        out
    }

    /// However a reply is cut into pieces, a direction split across them
    /// included, what's shown has none and what's spoken has what the voice
    /// performs: the same as the whole text would.
    #[test]
    fn a_split_stream_says_what_the_whole_text_would() {
        let text = "(excited) Found it! (pensive) Hmm. It broke (sad) again (sighing) twice. \
                    Ask Alice (API) or [warmly] me. (Laughs) Ha. Unclosed (end";
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            let whole = Parted {
                shown: markup.shown(text),
                spoken: markup.apply(text),
            };
            let chars: Vec<char> = text.chars().collect();
            for cut in 0..=chars.len() {
                let (a, b): (String, String) =
                    (chars[..cut].iter().collect(), chars[cut..].iter().collect());
                assert_eq!(parted(markup, &[&a, &b]), whole, "{markup:?} cut at {cut}");
            }
            let singles: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
            let refs: Vec<&str> = singles.iter().map(String::as_str).collect();
            assert_eq!(parted(markup, &refs), whole, "{markup:?} char by char");
        }
    }

    /// What's on screen and saved is the reply as written, less the directions
    /// that can be named: a reader must never lose content to a guess.
    #[test]
    fn what_is_shown_loses_nothing_but_named_directions() {
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            for kept in [
                "Either (a) this or (b) that.",
                "if (ready) { go(); }",
                "- [x] done\n- [ ] not yet",
                "Pass the flag (optional) too.",
                "See [docs](https://example.com/a) first.",
                "See [docs](guide) and [the notes][notes].",
                "Run `(excited)` and `[happy]` here.",
                "```text\n(excited) as written [happy]\n```\nDone.",
            ] {
                assert_eq!(parted(markup, &[kept]).shown, kept, "{markup:?}");
            }
        }
    }

    /// Code and link text are nobody's direction, for the voice either.
    #[test]
    fn code_and_link_text_are_left_alone_for_the_voice_too() {
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            for kept in [
                "See [excited](https://example.com/a) first.",
                "See [docs](guide) now.",
                "Run `(excited)` here.",
                "```\n(excited) [happy]\n```",
                "Either (a) this or (b) that.",
            ] {
                assert_eq!(markup.apply(kept), kept, "{markup:?}");
            }
        }
    }

    /// The same across every cut, a link cut at `](`, and a fence or inline code
    /// cut among its backticks.
    #[test]
    fn code_links_and_lists_part_the_same_at_every_cut() {
        let text = "(excited) Look: [docs](guide) and [x](https://e.com/a).\n\
                    - [x] done (sighing) mostly\n\
                    Run `(calm)` or ``a ` (sad) b``.\n\
                    ```rust\nif (ready) { f(\"(happy)\"); }\n```\n\
                    (laughing) Either (a) or (b).";
        let chars: Vec<char> = text.chars().collect();
        for markup in [Markup::FishS1, Markup::FishS2, Markup::Plain] {
            let whole = parted(markup, &[text]);
            assert!(whole.shown.contains("[docs](guide)"), "{}", whole.shown);
            assert!(whole.shown.contains("if (ready) { f(\"(happy)\"); }"));
            assert!(whole.shown.contains("`(calm)`"), "{}", whole.shown);
            assert!(whole.shown.contains("``a ` (sad) b``"), "{}", whole.shown);
            assert!(
                whole.shown.contains("Either (a) or (b)."),
                "{}",
                whole.shown
            );
            assert!(!whole.shown.contains("(excited)"), "{}", whole.shown);
            assert!(!whole.shown.contains("(sighing)"), "{}", whole.shown);
            assert!(!whole.shown.contains("(laughing)"), "{}", whole.shown);
            for cut in 0..=chars.len() {
                let (a, b): (String, String) =
                    (chars[..cut].iter().collect(), chars[cut..].iter().collect());
                assert_eq!(parted(markup, &[&a, &b]), whole, "{markup:?} cut at {cut}");
            }
            let singles: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
            let refs: Vec<&str> = singles.iter().map(String::as_str).collect();
            assert_eq!(parted(markup, &refs), whole, "{markup:?} char by char");
        }
    }

    #[test]
    fn what_is_shown_never_has_a_direction() {
        let out = parted(
            Markup::FishS1,
            &[
                "(conf",
                "ident) Found it. Well (sigh",
                "ing) yes. (la",
                "ughing) Ha.",
            ],
        );
        assert_eq!(out.shown, "Found it. Well yes. Ha.");
        assert_eq!(
            out.spoken,
            "(confident) Found it. Well (sighing) yes. (laughing) Ha."
        );
    }

    #[test]
    fn a_split_holds_back_only_what_might_be_a_direction() {
        let mut split = Markup::Plain.split();
        assert_eq!(split.push("Hello (exc").shown, "Hello");
        assert_eq!(split.push("ited) there").shown, " there");
        assert_eq!(split.push(" and f(x, y").shown, " and f(x, y");
        assert_eq!(split.push(" is ").shown, " is");
        assert_eq!(split.finish().shown, " ");
    }

    #[test]
    fn a_reset_split_drops_what_it_held() {
        let mut split = Markup::FishS1.split();
        assert_eq!(split.push("Okay. (exc").spoken, "Okay.");
        split.reset();
        assert_eq!(split.push("(excited) New.").spoken, "(excited) New.");
    }

    /// What was saved before directions were parted from the text may have
    /// been typed, so only what names a direction goes.
    #[test]
    fn a_kept_message_loses_only_named_directions() {
        assert_eq!(
            without_named_directions("(confident) It works (maybe). (Laughs) Ask (see below) me."),
            "It works (maybe). Ask (see below) me."
        );
        assert_eq!(
            without_named_directions("[excited] Found f(x)."),
            "Found f(x)."
        );
        assert_eq!(
            without_named_directions("Run `(excited)` and\n```\n(happy) x\n```\n(happy) Done."),
            "Run `(excited)` and\n```\n(happy) x\n```\nDone."
        );
        assert_eq!(without_named_directions("As typed."), "As typed.");
    }

    /// Words the tag table shares with ordinary writing stay where they're
    /// ordinary: this text may never have been a direction.
    #[test]
    fn a_kept_message_keeps_ordinary_words_where_they_are_ordinary() {
        for kept in [
            "Press the (pause) button.",
            "Use a (soft) delete.",
            "The (break) statement ends it.",
            "They were (happy) with it.",
            "See [docs](pause) now.",
        ] {
            assert_eq!(without_named_directions(kept), kept);
        }
        assert_eq!(without_named_directions("(pause) Well."), "Well.");
        assert_eq!(without_named_directions("Well (sighing) yes."), "Well yes.");
    }

    /// A voice told it may describe itself at the start of a sentence: that's as
    /// much a direction as a tag, on S2 alone, and a word elsewhere isn't.
    #[test]
    fn what_s2_may_describe_is_kept_from_the_reader() {
        let text = "[warmly] Welcome back. See [note] below.\n- [x] done";
        assert_eq!(
            Markup::FishS2.shown(text),
            "Welcome back. See [note] below.\n- [x] done"
        );
        assert_eq!(Markup::FishS1.shown(text), text);
    }

    /// Moods no voice performs don't reach the reader either.
    #[test]
    fn what_is_named_but_not_performed_is_not_shown() {
        for markup in [Markup::FishS1, Markup::Plain] {
            assert_eq!(markup.shown("(pensive) Hmm."), "Hmm.");
            assert_eq!(markup.apply("(pensive) Hmm."), "Hmm.");
        }
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
