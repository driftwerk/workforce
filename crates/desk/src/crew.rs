//! The crew at a glance: what happened (the activity log, always recorded),
//! where everyone is (the overview), and what the overseeing agent proposed
//! (suggestions). Sessions are recorded by the desk itself; agents' turns,
//! the overseer and its actions by the hub through [`Crew`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Engine, Origin, SessionState, SessionView};

/// Who or what an event is by or about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartyKind {
    Agent,
    Session,
    Manager,
    You,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub kind: PartyKind,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub engine: Option<Engine>,
    #[serde(default)]
    pub origin: Option<Origin>,
}

impl Party {
    pub fn you() -> Self {
        Self {
            kind: PartyKind::You,
            id: "you".into(),
            name: "You".into(),
            engine: None,
            origin: None,
        }
    }

    pub fn agent(id: &str, name: &str) -> Self {
        Self {
            kind: PartyKind::Agent,
            id: id.into(),
            name: name.into(),
            engine: None,
            origin: None,
        }
    }

    pub fn manager(id: &str, name: &str) -> Self {
        Self {
            kind: PartyKind::Manager,
            ..Self::agent(id, name)
        }
    }

    pub fn session(v: &SessionView) -> Self {
        Self {
            kind: PartyKind::Session,
            id: v.id.to_string(),
            name: v.name.clone(),
            engine: Some(v.engine),
            origin: Some(v.origin),
        }
    }
}

macro_rules! kinds {
    ($($variant:ident => $text:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Kind { $($variant),+ }

        impl Kind {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

kinds! {
    Started => "started",
    Working => "working",
    NeedsYou => "needs_you",
    Question => "question",
    Answered => "answered",
    Finished => "finished",
    Failed => "failed",
    Limit => "limit",
    Quiet => "quiet",
    Stopped => "stopped",
    Resumed => "resumed",
    Restarted => "restarted",
    Nudged => "nudged",
    Approved => "approved",
    Denied => "denied",
    Suggested => "suggested",
    Dismissed => "dismissed",
    Summary => "summary",
    Assigned => "assigned",
}

/// One thing that happened on the crew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub at: DateTime<Utc>,
    pub kind: Kind,
    pub actor: Party,
    #[serde(default)]
    pub subject: Option<Party>,
    pub text: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub suggestion_id: Option<String>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Agent-to-agent hand-offs behind this event (the loop guard's count);
    /// the hub's own business, not on the wire.
    #[serde(skip)]
    pub hop: i16,
    /// What the overview reads back about the event: the task, the answer,
    /// the question and its choices. Not on the wire.
    #[serde(skip)]
    pub detail: Option<Value>,
}

/// Now, to the microsecond Postgres keeps.
pub fn now() -> DateTime<Utc> {
    let now = Utc::now();
    DateTime::from_timestamp_micros(now.timestamp_micros()).unwrap_or(now)
}

impl Event {
    /// A new event, now. `text` reads as what the actor did; for the
    /// overseer and you it names the subject.
    pub fn new(kind: Kind, actor: Party, subject: Option<Party>, text: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            at: now(),
            kind,
            actor,
            subject,
            text: text.into(),
            reason: None,
            suggestion_id: None,
            approval_id: None,
            hop: 0,
            detail: None,
        }
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        self.reason = (!reason.trim().is_empty()).then_some(reason);
        self
    }

    pub fn approval(mut self, id: impl ToString) -> Self {
        self.approval_id = Some(id.to_string());
        self
    }

    pub fn suggestion(mut self, id: Option<String>) -> Self {
        self.suggestion_id = id;
        self
    }

    pub fn hop(mut self, hop: i16) -> Self {
        self.hop = hop;
        self
    }

    /// Whether the "Needs you" filter shows it.
    pub fn needs_you(&self) -> bool {
        matches!(self.kind, Kind::NeedsYou | Kind::Question)
    }
}

/// Who is acting through the desk, and why: you through Desk's routes, the
/// overseer through its tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    pub party: Party,
    pub reason: Option<String>,
    pub suggestion_id: Option<String>,
    pub hop: i16,
}

impl By {
    pub fn you() -> Self {
        Self {
            party: Party::you(),
            reason: None,
            suggestion_id: None,
            hop: 0,
        }
    }

    pub fn manager(id: &str, name: &str, reason: &str) -> Self {
        Self {
            party: Party::manager(id, name),
            reason: Some(reason.to_string()).filter(|r| !r.trim().is_empty()),
            suggestion_id: None,
            hop: 0,
        }
    }

    /// The event this actor's `kind` of action on `subject` makes.
    pub fn event(&self, kind: Kind, subject: Option<Party>, text: impl Into<String>) -> Event {
        let mut e = Event::new(kind, self.party.clone(), subject, text).hop(self.hop);
        e.reason = self.reason.clone();
        e.suggestion_id = self.suggestion_id.clone();
        e
    }
}

/// Which events a page of Activity holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    #[default]
    All,
    NeedsYou,
    Manager,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActivityPage {
    pub events: Vec<Event>,
    pub more: bool,
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID: 26 characters that sort as the time they were made.
pub fn new_id() -> String {
    let ms = Utc::now().timestamp_millis().max(0) as u128;
    let random = u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes()) & ((1u128 << 80) - 1);
    let mut n = (ms << 80) | random;
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(n & 31) as usize];
        n >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("ascii")
}

// ── The overview ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    NeedsYou,
    Working,
    Quiet,
    Idle,
    Done,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Session,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemApproval {
    pub id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemQuestion {
    pub text: String,
    pub options: Option<Vec<String>>,
}

/// An agent or a session as the overview shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: ItemKind,
    pub id: String,
    pub name: String,
    pub engine: Option<Engine>,
    pub origin: Option<Origin>,
    pub state: ItemState,
    pub task: Option<String>,
    pub detail: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    pub approval: Option<ItemApproval>,
    pub question: Option<ItemQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Who {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub text: String,
    pub at: DateTime<Utc>,
    pub by: Who,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub working: usize,
    pub needs_you: usize,
    pub done_today: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub brief: Option<Brief>,
    pub counts: Counts,
    pub items: Vec<Item>,
}

/// The overseer, as `GET /api/desk/manager` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerView {
    pub agent_id: Option<String>,
    pub level: Option<String>,
}

/// The overseer as the desk needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overseer {
    pub id: String,
    pub name: String,
    /// `watch`, `suggest` or `act`.
    pub level: String,
    /// Working longer than this without a sign of life is quiet; 0 never.
    pub quiet_minutes: u32,
}

/// An agent's turn running now, as the hub sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTurn {
    pub task: Option<String>,
    pub detail: Option<String>,
    pub since: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// One of the hub's agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewAgent {
    pub id: String,
    pub name: String,
    pub live: Option<LiveTurn>,
}

// ── Suggestions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionState {
    Open,
    Done,
    Dismissed,
}

impl SuggestionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "dismissed" => Self::Dismissed,
            _ => Self::Open,
        }
    }
}

/// What the overseer proposed: `action` is the tool call it would have made
/// (`{"tool": "crew.restart", "input": {…}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub at: DateTime<Utc>,
    pub text: String,
    pub action: Value,
    pub state: SuggestionState,
    pub subject: Option<Party>,
    #[serde(skip)]
    pub agent_id: String,
}

/// What an overseer's DM message mentions to act on in place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extras {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<MentionedApproval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<MentionedSuggestion>,
}

impl Extras {
    pub fn is_empty(&self) -> bool {
        self.approvals.is_empty() && self.suggestions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedApproval {
    pub session_id: String,
    pub approval_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionedSuggestion {
    pub id: String,
    pub text: String,
    pub state: SuggestionState,
}

/// The hub's side of the crew: its agents, the overseer, and doing what a
/// suggestion proposed.
#[async_trait::async_trait]
pub trait Crew: Send + Sync {
    /// The agent overseeing the crew, if one does.
    fn overseer(&self) -> Option<Overseer>;
    /// The hub's agents, with the turn each is running.
    fn agents(&self) -> Vec<CrewAgent>;
    /// The start of today, in the person's time.
    fn midnight(&self) -> DateTime<Utc>;
    /// Does what `suggestion` proposed, as you; what went wrong in plain
    /// words.
    async fn accept(&self, suggestion: &Suggestion) -> Result<(), String>;
}

// ── Words ────────────────────────────────────────────────────────────────

/// The first sentence (or line) of `text`, flattened and clipped to `max`
/// characters.
pub fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let sentence = line
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, c)| &line[..i + c.len_utf8()])
        .unwrap_or(line);
    clip(sentence, max)
}

/// `text` cut to `max` characters, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The question a reply ends on, to the person: its last sentence when that
/// asks something. Code blocks and trailing sign-offs are not questions.
pub fn question_in(reply: &str) -> Option<String> {
    let mut prose = String::new();
    let mut fenced = false;
    for line in reply.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    let last = prose.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    if !last.trim_end_matches(['*', '_', ')']).ends_with('?') {
        return None;
    }
    // The sentence the line ends on.
    let body = last.trim_end_matches(['*', '_']);
    let start = body[..body.len() - 1]
        .char_indices()
        .filter(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && body[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .unwrap_or(0);
    let q = body[start..]
        .trim()
        .trim_start_matches(['-', '*', '>'])
        .trim();
    (!q.is_empty()).then(|| clip(q, 240))
}

/// The choices a question offers, when they are clear: code or bold spans
/// it names, the short list just before it, or "A or B" between two single
/// words.
pub fn options_in(reply: &str, question: &str) -> Option<Vec<String>> {
    let spans = spans(question);
    if spans.len() >= 2 && spans.len() <= 6 {
        return Some(spans);
    }
    // A short list right before the question.
    let lines: Vec<&str> = reply.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .rposition(|l| l.contains(question.trim_end_matches('?')))?;
    let mut listed = Vec::new();
    for l in lines[..at].iter().rev() {
        if l.is_empty() && listed.is_empty() {
            continue;
        }
        let item = l
            .strip_prefix("- ")
            .or_else(|| l.strip_prefix("* "))
            .or_else(|| {
                let (n, rest) = l.split_once(". ").or_else(|| l.split_once(") "))?;
                n.chars().all(|c| c.is_ascii_digit()).then_some(rest)
            });
        match item {
            Some(item) if item.chars().count() <= 80 => listed.push(clean(item)),
            _ => break,
        }
    }
    if (2..=6).contains(&listed.len()) {
        listed.reverse();
        return Some(listed);
    }
    either_or(question)
}

fn clean(s: &str) -> String {
    s.trim()
        .trim_matches(|c| matches!(c, '`' | '*' | '_' | '"' | '“' | '”'))
        .trim()
        .to_string()
}

/// `code` and **bold** spans, in order, each once.
fn spans(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for marker in ["`", "**"] {
        let parts: Vec<&str> = text.split(marker).collect();
        if parts.len() >= 5 {
            for span in parts.iter().skip(1).step_by(2) {
                let span = span.trim();
                if !span.is_empty() && span.chars().count() <= 60 && !out.iter().any(|o| o == span)
                {
                    out.push(span.to_string());
                }
            }
            if out.len() >= 2 {
                return out;
            }
            out.clear();
        }
    }
    out
}

const NOT_CHOICES: &[&str] = &[
    "it",
    "this",
    "that",
    "them",
    "not",
    "now",
    "later",
    "so",
    "too",
    "else",
    "either",
    "both",
    "something",
    "anything",
    "you",
    "me",
    "i",
    "we",
];

/// "deploy main or release/4.2 to staging?" → main, release/4.2.
fn either_or(question: &str) -> Option<Vec<String>> {
    let body = question.trim_end_matches('?');
    let mut halves = body.split(" or ");
    let (left, right) = (halves.next()?, halves.next()?);
    if halves.next().is_some() {
        return None;
    }
    let a = clean(left.split_whitespace().last()?.trim_end_matches(','));
    let b = clean(right.split_whitespace().next()?.trim_end_matches(','));
    let fine = |w: &str| {
        !w.is_empty()
            && w.chars().count() <= 40
            && !NOT_CHOICES.contains(&w.to_lowercase().as_str())
            && !w.ends_with("ing")
            && !w.ends_with("ed")
    };
    (fine(&a) && fine(&b) && a != b).then(|| vec![a, b])
}

/// Whether a reply says the work stopped on a usage or rate limit.
pub fn hit_limit(reply: &str) -> bool {
    let r = reply.to_lowercase();
    [
        "usage limit",
        "limit reached",
        "rate limit",
        "limit will reset",
        "out of extra usage",
    ]
    .iter()
    .any(|p| r.contains(p))
}

/// What a session's pending request asks, as a question: "Run npm install …?"
pub fn approval_summary(tool: &str, detail: &str) -> String {
    let name = tool.split('(').next().unwrap_or(tool).trim();
    let detail = detail.trim();
    match name {
        "Input" => detail.to_string(),
        "Bash" if !detail.is_empty() => format!("Run {detail}?"),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" if !detail.is_empty() => {
            format!("Edit {detail}?")
        }
        "Read" if !detail.is_empty() => format!("Read {detail}?"),
        "WebFetch" if !detail.is_empty() => format!("Fetch {detail}?"),
        _ if detail.is_empty() => format!("Use {name}?"),
        _ => format!("Use {name} on {detail}?"),
    }
}

/// The overview's word for a session's state.
pub fn session_state(state: SessionState) -> ItemState {
    match state {
        SessionState::Blocked => ItemState::NeedsYou,
        SessionState::Starting | SessionState::Working => ItemState::Working,
        SessionState::Done => ItemState::Done,
        SessionState::Idle => ItemState::Idle,
        SessionState::Interrupted | SessionState::Stopped | SessionState::Closed => {
            ItemState::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_as_they_were_made() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(a < b, "{a} {b}");
    }

    #[test]
    fn a_reply_ending_on_a_question_asks_it() {
        let reply = "I've built release/4.2.\n\nShould I deploy main or release/4.2 to staging?";
        let q = question_in(reply).unwrap();
        assert_eq!(q, "Should I deploy main or release/4.2 to staging?");
        assert_eq!(
            options_in(reply, &q),
            Some(vec!["main".to_string(), "release/4.2".to_string()])
        );
        assert_eq!(question_in("All green. Pushed."), None);
        assert_eq!(
            question_in("Done.\n```sh\nwhy?\n```\nAll good."),
            None,
            "code is not a question"
        );
        assert_eq!(
            question_in("Tests pass. Want me to open the PR?").as_deref(),
            Some("Want me to open the PR?")
        );
    }

    #[test]
    fn clear_choices_are_offered_and_vague_ones_are_not() {
        let q = "Which one: `blue` or `green`?";
        assert_eq!(
            options_in(q, q),
            Some(vec!["blue".to_string(), "green".to_string()])
        );
        let listed = "I can:\n- Roll back\n- Patch forward\n\nWhich would you like?";
        assert_eq!(
            options_in(listed, &question_in(listed).unwrap()),
            Some(vec!["Roll back".to_string(), "Patch forward".to_string()])
        );
        let vague = "Should I keep going or stop?";
        assert_eq!(options_in(vague, vague), None);
        let open = "What should the release notes say?";
        assert_eq!(options_in(open, open), None);
    }

    #[test]
    fn one_line_is_the_first_sentence_clipped() {
        assert_eq!(one_line("## Plan\nCompare spend. Then report.", 80), "Plan");
        assert_eq!(
            one_line("Compare spend. Then report.", 80),
            "Compare spend."
        );
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn requests_read_as_questions() {
        assert_eq!(
            approval_summary(
                "Bash(npm install @acme/ds@4.2.0)",
                "npm install @acme/ds@4.2.0"
            ),
            "Run npm install @acme/ds@4.2.0?"
        );
        assert_eq!(approval_summary("Edit(a.rs)", "a.rs"), "Edit a.rs?");
        assert_eq!(
            approval_summary("Input", "Claude needs your permission"),
            "Claude needs your permission"
        );
    }
}
