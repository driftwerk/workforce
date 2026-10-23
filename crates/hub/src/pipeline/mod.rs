//! Pipelines: the hub's first-class notion of "items moving through stages".
//!
//! A pipeline is two things in the document store, both in one scope:
//!
//! - a **definition** — `pipelines/<id>` — that names the collection its
//!   items live in, the lanes an item moves through, the exits it can take,
//!   and the rules that put an event on a column; and
//! - one **record per item** in that collection — the current stage, a
//!   block per stage with what that stage last did and which run did it,
//!   and a bounded history of events.
//!
//! Everything that moves an item goes through [`record_event`], so every
//! pipeline's records have one shape and one concurrency story. Everything
//! that reads one goes through the definition, so a board, a grade or a
//! report never has to know a particular pipeline's vocabulary. The hub
//! knows what *a* pipeline is; which pipelines exist is data.

pub mod board;
pub mod costs;
pub mod facts;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod notices;
pub mod position;
pub mod runs;
pub mod timings;

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub use crate::hub_core::pipelines::HubReads;
use crate::hub_core::pipelines::{Scope, StoreError};
use crate::store::{Document, DocumentStore};

/// `text` cut to at most `max` characters, an ellipsis marking the cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some(_) => {
            let kept: String = text.chars().take(max.saturating_sub(1)).collect();
            format!("{}…", kept.trim_end())
        }
    }
}

/// The longest label a `label_field` yields, in characters.
pub const LABEL_MAX: usize = 120;

/// A detail value as label text: one line, without `<` or `>` so no Slack
/// mention or link markup survives, at most [`LABEL_MAX`] characters. `None`
/// when the value is not text or holds nothing to show.
pub fn label_text(value: &Value) -> Option<String> {
    let mapped: String = value
        .as_str()?
        .chars()
        .filter(|c| !matches!(c, '<' | '>'))
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let line = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| clip(&line, LABEL_MAX))
}

/// The detail of an event that records none.
pub(crate) static NO_DETAIL: Value = Value::Null;

/// An item record's history, oldest first.
pub(crate) fn history(body: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    body.get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The collection, in any scope, that holds that scope's definitions.
pub const DEFINITIONS: &str = "pipelines";
/// Events an item record keeps; older ones fall off the front.
pub const HISTORY_MAX: usize = 200;
/// Compare-and-swap attempts before a record write gives up.
const RECORD_TRIES: usize = 4;

// ---------------------------------------------------------------------------
// Definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Active,
    Good,
    /// Waiting on a person: something here needs a human to act.
    Warn,
    /// The pipeline is still working on it — retrying, re-checking, sent back
    /// to fix itself. Nothing for a person to do yet, unlike `Warn`.
    Healing,
    Bad,
    #[default]
    Neutral,
}

impl Tone {
    /// A person must act on an item with this tone: it waits on one, or it halted.
    pub fn needs_person(self) -> bool {
        matches!(self, Tone::Warn | Tone::Bad)
    }
}

impl std::fmt::Display for Tone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Tone::Active => "active",
            Tone::Good => "good",
            Tone::Warn => "warn",
            Tone::Healing => "healing",
            Tone::Bad => "bad",
            Tone::Neutral => "neutral",
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub id: String,
    pub label: String,
    /// How long a finished item stays on the board after entering this column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
}

/// The cards a column shows before it scrolls, when the definition says nothing.
pub const DEFAULT_CARDS: u32 = 10;
pub const MAX_CARDS: u32 = 50;
/// The shortest window: it hides finished items that pile up over days.
pub const WINDOW_MIN_HOURS: u64 = 24;
pub const WINDOW_MAX_DAYS: u64 = 3650;

/// How the board lays the pipeline out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    /// Cards a column shows before it scrolls; [`DEFAULT_CARDS`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cards: Option<u32>,
}

/// A span of `<n>h`, `<n>d` or `all`, kept in the spelling it was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Window {
    Hours(u32),
    Days(u32),
    All,
}

impl Window {
    /// How far back the window reaches; `None` for `all`.
    pub fn duration(self) -> Option<chrono::Duration> {
        match self {
            Window::Hours(n) => Some(chrono::Duration::hours(n.into())),
            Window::Days(n) => Some(chrono::Duration::days(n.into())),
            Window::All => None,
        }
    }
}

impl std::str::FromStr for Window {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        if text == "all" {
            return Ok(Window::All);
        }
        let Some((split, unit)) = text.char_indices().last() else {
            return Err("a window may not be empty; write `<n>h`, `<n>d` or `all`".into());
        };
        let (number, hours_each) = match unit {
            'h' => (&text[..split], 1),
            'd' => (&text[..split], 24),
            _ if text.bytes().all(|b| b.is_ascii_digit()) => {
                return Err(format!(
                    "`{text}` needs a unit: `h` for hours or `d` for days"
                ))
            }
            _ => return Err(format!("`{text}` is not `<n>h`, `<n>d` or `all`")),
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("`{text}` needs a whole number before `{unit}`"));
        }
        if number.len() > 1 && number.starts_with('0') {
            return Err(format!("`{text}` starts with a zero"));
        }
        let too_long = || format!("`{text}` is longer than {WINDOW_MAX_DAYS} days");
        let n: u32 = number.parse().map_err(|_| too_long())?;
        match u64::from(n) * hours_each {
            hours if hours < WINDOW_MIN_HOURS => {
                Err(format!("`{text}` is shorter than {WINDOW_MIN_HOURS} hours"))
            }
            hours if hours > WINDOW_MAX_DAYS * 24 => Err(too_long()),
            _ if unit == 'h' => Ok(Window::Hours(n)),
            _ => Ok(Window::Days(n)),
        }
    }
}

impl TryFrom<String> for Window {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        text.parse()
    }
}

impl From<Window> for String {
    fn from(window: Window) -> Self {
        window.to_string()
    }
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Window::Hours(n) => write!(f, "{n}h"),
            Window::Days(n) => write!(f, "{n}d"),
            Window::All => f.write_str("all"),
        }
    }
}

/// One way an event places or annotates an item; the first rule whose `when`
/// holds wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Rule {
    /// Every key must equal the event's `detail[key]`, compared as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<BTreeMap<String, String>>,
    /// A lane id (the item stays in the flow) or an exit id (it parks).
    /// Absent on an annotation rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The event is noted on the item without moving it: its history and
    /// stage block take the event, its place and current stage do not change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub annotation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
    /// Shown on the item instead of the column's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The `detail` field whose non-empty text replaces `label`; see
    /// [`label_text`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_field: Option<String>,
    /// The `detail` field naming the rung this event is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung_field: Option<String>,
    /// The rung named by `rung_field` counts as passed after this event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rung_passed: bool,
    /// A run is working the item: the claim holds while this event is the
    /// item's latest placement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claim: bool,
    /// The `detail` field holding the claim's lease in minutes; past it the
    /// claim no longer holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_field: Option<String>,
    /// The `detail` field holding when the item is next due (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_field: Option<String>,
}

impl Rule {
    /// The column this rule puts an item on; `None` for an annotation.
    pub fn places(&self) -> Option<&str> {
        self.column.as_deref().filter(|_| !self.annotation)
    }

    /// The fields only a placing rule may set, among those this rule sets.
    fn placing_fields(&self) -> impl Iterator<Item = &'static str> {
        [
            ("column", self.column.is_some()),
            ("claim", self.claim),
            ("lease_field", self.lease_field.is_some()),
            ("due_field", self.due_field.is_some()),
            ("rung_field", self.rung_field.is_some()),
            ("rung_passed", self.rung_passed),
        ]
        .into_iter()
        .filter_map(|(field, set)| set.then_some(field))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkField {
    pub field: String,
    pub label: String,
}

/// The moment an item came into being outside the pipeline, as a source
/// records it on the item's events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// The `detail` field holding the moment, e.g. `filed_at`.
    pub field: String,
    /// What the moment is called: "Filed", "Ordered".
    pub label: String,
}

/// How an item's time is read: where its clock starts, which lanes are
/// timed from there, and where the clock stops.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Absent: the clock starts when the pipeline first recorded the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// Lane ids timed from the origin. Absent: the last lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestones: Option<Vec<String>>,
    /// Lane or exit ids where the clock stops. Absent: the last lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How the pipeline works, in markdown: what each lane means, what heals
    /// itself and what needs a person. Shown on the board above the definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<String>,
    /// What one item is called: "ticket", "order", "candidate". Default "item".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_label: Option<String>,
    /// The collection, in the same scope, holding one record per item.
    pub collection: String,
    pub lanes: Vec<Column>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exits: Vec<Column>,
    /// The rung ladder shown on items whose position names a rung, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rungs: Vec<String>,
    /// How an item's time is read; see [`Timings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<Timings>,
    /// How the board lays the pipeline out; see [`Board`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<Board>,
    /// `"<stage>/<event>"` (or `"<stage>/*"`) → rules. An event with no rules leaves the item where it is.
    pub events: BTreeMap<String, Vec<Rule>>,
    /// Dotted path into the record for its one-line summary, e.g. `stages.implement.summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_field: Option<String>,
    /// Fields worth a link on the item, e.g. a PR URL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_fields: Vec<LinkField>,
    /// Which workflows or skills feed this pipeline — informational, for people and the AI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// The author's own revision counter. A skill that ships its definition
    /// bumps this when the definition changes and redefines only when the
    /// stored revision is older, so a hand edit in between is not undone
    /// silently and an unchanged definition is never rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

/// What `validate` found. Errors refuse the definition; warnings are
/// returned with it so the author can decide.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Findings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Findings {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// A guide is a page, not a book: past this a person would not read it and a
/// definition document would carry it on every read.
pub const GUIDE_MAX_BYTES: usize = 64 * 1024;

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_event_key(k: &str) -> bool {
    match k.split_once('/') {
        Some((stage, event)) => valid_id(stage) && (event == "*" || valid_id(event)),
        None => false,
    }
}

impl PipelineDefinition {
    /// Every problem a person or the AI would want named before a definition is stored.
    pub fn validate(&self) -> Findings {
        let mut f = Findings::default();
        if self.name.trim().is_empty() {
            f.errors.push("`name` is required".into());
        }
        if let Err(e) = crate::store::validate_segment("collection", &self.collection) {
            f.errors.push(e.to_string());
        }
        if self.collection == DEFINITIONS {
            f.errors.push(format!("`collection` may not be `{DEFINITIONS}` — that collection holds the definitions themselves"));
        }
        if self.collection == notices::COLLECTION {
            f.errors.push(format!(
                "`collection` may not be `{}` — that collection holds pipeline notices",
                notices::COLLECTION
            ));
        }
        if self.lanes.is_empty() {
            f.errors.push("`lanes` needs at least one lane".into());
        }
        if let Some(bytes) = self
            .guide
            .as_ref()
            .map(String::len)
            .filter(|n| *n > GUIDE_MAX_BYTES)
        {
            f.errors.push(format!(
                "`guide` is {bytes} bytes; the limit is {GUIDE_MAX_BYTES}"
            ));
        }
        let mut seen = HashSet::new();
        for (kind, cols) in [("lane", &self.lanes), ("exit", &self.exits)] {
            for c in cols {
                if !valid_id(&c.id) {
                    f.errors.push(format!(
                        "{kind} id `{}` must be lowercase letters, digits, - or _",
                        c.id
                    ));
                }
                if c.label.trim().is_empty() {
                    f.errors.push(format!("{kind} `{}` needs a label", c.id));
                }
                if !seen.insert(c.id.clone()) {
                    f.errors
                        .push(format!("`{}` is used twice as a lane or exit id", c.id));
                }
            }
        }
        if self.events.is_empty() {
            f.errors.push(
                "`events` needs at least one `stage/event` entry — nothing could ever move".into(),
            );
        }
        let mut lands_in: HashSet<&str> = HashSet::new();
        for (key, rules) in &self.events {
            if !valid_event_key(key) {
                f.errors.push(format!(
                    "event key `{key}` must be `stage/event` (or `stage/*`) in lowercase ids"
                ));
            }
            if rules.is_empty() {
                f.errors
                    .push(format!("`{key}` has no rules; drop the entry or add one"));
            }
            for (i, r) in rules.iter().enumerate() {
                let n = i + 1;
                if r.annotation {
                    f.errors.extend(r.placing_fields().map(|field| {
                        format!("`{key}` rule {n} is an annotation, which never moves the item, so it may not set `{field}`")
                    }));
                } else {
                    match r.column.as_deref() {
                        Some(column) => {
                            if !seen.contains(column) {
                                f.errors.push(format!("`{key}` rule {n} sends the item to `{column}`, which is neither a lane nor an exit"));
                            }
                            lands_in.insert(column);
                        }
                        None => f.errors.push(format!(
                            "`{key}` rule {n} needs a `column`, or `annotation: true` to note the event without moving the item"
                        )),
                    }
                    if r.rung_passed && r.rung_field.is_none() {
                        f.errors.push(format!(
                            "`{key}` rule {n} sets `rung_passed` without a `rung_field`"
                        ));
                    }
                    if r.rung_field.is_some() && self.rungs.is_empty() {
                        f.warnings.push(format!(
                            "`{key}` names a `rung_field` but the definition has no `rungs` ladder"
                        ));
                    }
                    if r.claim && r.due_field.is_some() {
                        f.errors.push(format!(
                            "`{key}` rule {n} sets both `claim` and `due_field`; a claimed item is being worked, not waiting"
                        ));
                    }
                    if r.lease_field.is_some() && !r.claim {
                        f.errors.push(format!(
                            "`{key}` rule {n} sets `lease_field` without `claim`"
                        ));
                    }
                }
                f.errors.extend(
                    [
                        ("lease_field", &r.lease_field),
                        ("due_field", &r.due_field),
                        ("label_field", &r.label_field),
                    ]
                    .into_iter()
                    .filter_map(|(name, field)| {
                        let what = format!("`{key}` rule {n} `{name}`");
                        crate::store::validate_segment(&what, field.as_deref()?).err()
                    })
                    .map(|e| e.to_string()),
                );
                if let Some(when) = &r.when {
                    if when.is_empty() {
                        f.warnings.push(format!(
                            "`{key}` rule {} has an empty `when`; it always matches",
                            i + 1
                        ));
                    }
                }
                if i + 1 < rules.len() && r.when.as_ref().is_none_or(|w| w.is_empty()) {
                    f.warnings.push(format!(
                        "`{key}` rule {} has no `when`, so the rules after it can never match",
                        i + 1
                    ));
                }
            }
        }
        for c in &self.lanes {
            if !lands_in.contains(c.id.as_str()) {
                f.warnings
                    .push(format!("no event ever lands in lane `{}`", c.id));
            }
        }
        for c in &self.exits {
            if !lands_in.contains(c.id.as_str()) {
                f.warnings
                    .push(format!("no event ever lands in exit `{}`", c.id));
            }
        }
        if let Some(sf) = &self.summary_field {
            if crate::store::validate_field(sf).is_err() {
                f.errors.push(format!(
                    "`summary_field` `{sf}` must be a dotted path of plain keys"
                ));
            }
        }
        for l in &self.link_fields {
            if l.field.trim().is_empty() || l.label.trim().is_empty() {
                f.errors
                    .push("each `link_fields` entry needs a `field` and a `label`".into());
            }
        }
        if let Some(timings) = &self.timings {
            self.check_timings(timings, &mut f);
        }
        self.check_board(&mut f);
        f
    }

    fn check_board(&self, f: &mut Findings) {
        if let Some(cards) = self
            .board
            .as_ref()
            .and_then(|board| board.cards)
            .filter(|cards| !(1..=MAX_CARDS).contains(cards))
        {
            f.errors.push(format!(
                "`board.cards` is {cards}; it must be between 1 and {MAX_CARDS}"
            ));
        }
        let ends = self.ends();
        let windowed: Vec<(&str, &Column)> = [("lane", &self.lanes), ("exit", &self.exits)]
            .into_iter()
            .flat_map(|(kind, columns)| columns.iter().map(move |column| (kind, column)))
            .filter(|(_, column)| column.window.is_some())
            .collect();
        f.errors.extend(
            windowed
                .iter()
                .filter(|(_, column)| !ends.contains(&column.id.as_str()))
                .map(|(kind, column)| {
                    format!(
                        "{kind} `{}` sets `window`, but an item's clock does not stop there (`timings.ends`); only finished items may be hidden",
                        column.id
                    )
                }),
        );
        let windowed = &windowed;
        f.errors.extend(self.events.iter().flat_map(|(key, rules)| {
            rules.iter().enumerate().filter_map(move |(i, rule)| {
                let column = rule.places()?;
                let tone = rule.tone.filter(|tone| tone.needs_person())?;
                windowed.iter().any(|(_, c)| c.id == column).then(|| {
                    format!(
                        "`{key}` rule {} sends the item to `{column}` with tone `{tone}`, but `{column}` sets `window`; an item that needs a person must never be hidden",
                        i + 1
                    )
                })
            })
        }));
    }

    /// Cards a column shows before it scrolls.
    pub fn cards(&self) -> u32 {
        self.board
            .as_ref()
            .and_then(|board| board.cards)
            .unwrap_or(DEFAULT_CARDS)
    }

    fn check_timings(&self, timings: &Timings, f: &mut Findings) {
        if let Some(origin) = &timings.origin {
            if let Err(e) = crate::store::validate_segment("`timings.origin.field`", &origin.field)
            {
                f.errors.push(e.to_string());
            }
            if origin.label.trim().is_empty() {
                f.errors.push("`timings.origin` needs a `label`".into());
            }
        }
        if timings.milestones.as_ref().is_some_and(Vec::is_empty) {
            f.warnings.push(
                "an empty `timings.milestones` reports none; omit it for the last lane".into(),
            );
        }
        let is_exit = |id: &str| self.exits.iter().any(|e| e.id == id);
        f.errors.extend(
            timings
                .milestones
                .iter()
                .flatten()
                .filter(|id| !self.is_lane(id))
                .map(|id| {
                    if is_exit(id) {
                        format!("`timings.milestones` names `{id}`, which is an exit; milestones are lanes")
                    } else {
                        format!("`timings.milestones` names `{id}`, which is not a lane")
                    }
                }),
        );
        f.errors.extend(
            timings
                .ends
                .iter()
                .flatten()
                .filter(|id| !self.is_lane(id) && !is_exit(id))
                .map(|id| {
                    format!("`timings.ends` names `{id}`, which is neither a lane nor an exit")
                }),
        );
        for (name, ids) in [("milestones", &timings.milestones), ("ends", &timings.ends)] {
            let mut seen = HashSet::new();
            f.errors.extend(
                ids.iter()
                    .flatten()
                    .filter(|id| !seen.insert(id.as_str()))
                    .map(|id| format!("`timings.{name}` names `{id}` more than once")),
            );
        }
    }

    pub fn is_lane(&self, column: &str) -> bool {
        self.lane_index(column).is_some()
    }

    /// Where `column` sits in the flow, when it is a lane.
    pub fn lane_index(&self, column: &str) -> Option<usize> {
        self.lanes.iter().position(|l| l.id == column)
    }

    /// The lanes an item's time is reported against, in lane order.
    pub fn milestones(&self) -> Vec<&Column> {
        match self.timings.as_ref().and_then(|t| t.milestones.as_ref()) {
            Some(ids) => self.lanes.iter().filter(|l| ids.contains(&l.id)).collect(),
            None => self.lanes.last().into_iter().collect(),
        }
    }

    /// The lane and exit ids where an item's clock stops.
    pub fn ends(&self) -> Vec<&str> {
        match self.timings.as_ref().and_then(|t| t.ends.as_ref()) {
            Some(ids) => ids.iter().map(String::as_str).collect(),
            None => self
                .lanes
                .last()
                .map(|l| l.id.as_str())
                .into_iter()
                .collect(),
        }
    }

    pub fn column_label(&self, column: &str) -> String {
        self.lanes
            .iter()
            .chain(self.exits.iter())
            .find(|c| c.id == column)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| column.to_string())
    }

    /// The rule an event resolves to, or none when the definition says nothing about it.
    pub fn rule_for(&self, stage: &str, event: &str, detail: &Value) -> Option<&Rule> {
        let rules = self
            .events
            .get(&format!("{stage}/{event}"))
            .or_else(|| self.events.get(&format!("{stage}/*")))?;
        rules.iter().find(|r| {
            r.when
                .as_ref()
                .is_none_or(|when| detail_matches(when, detail))
        })
    }

    /// What an event under `rule` is called on the board: its `label_field`
    /// text, the rule's label, its column's label, else the event itself.
    pub fn label_of(&self, rule: &Rule, stage: &str, event: &str, detail: &Value) -> String {
        rule.label_field
            .as_deref()
            .and_then(|field| label_text(detail.get(field)?))
            .or_else(|| rule.label.clone())
            .unwrap_or_else(|| match &rule.column {
                Some(column) => self.column_label(column),
                None => format!("{stage}/{event}"),
            })
    }
}

/// Whether every key of `when` equals `detail`'s field, compared as text.
fn detail_matches(when: &BTreeMap<String, String>, detail: &Value) -> bool {
    when.iter().all(|(k, v)| match detail.get(k) {
        Some(Value::String(s)) => s == v,
        Some(Value::Null) | None => false,
        Some(other) => other.to_string().as_str() == v.as_str(),
    })
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// An event as callers hand it in.
#[derive(Debug, Clone, Deserialize)]
pub struct EventInput {
    pub item: String,
    pub stage: String,
    pub event: String,
    #[serde(default)]
    pub detail: Value,
    /// The run (workflow instance) doing the recording, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// The record body after an event: `stage` becomes the current stage unless
/// `rule`, the rule the definition resolves the event to, makes it an
/// annotation; the stage's block takes the detail (later events overwrite
/// earlier keys) plus the run and time; and the event joins the history,
/// oldest dropped past the cap. Pure, so the in-memory and Postgres paths
/// agree.
pub fn apply_event(mut body: Value, e: &EventInput, at: &str, rule: Option<&Rule>) -> Value {
    if !body.is_object() {
        body = json!({});
    }
    let detail = match &e.detail {
        Value::Object(m) => Value::Object(m.clone()),
        Value::String(s) if !s.trim().is_empty() => json!({ "detail": s.trim() }),
        _ => json!({}),
    };
    body["item"] = json!(e.item);
    if rule.is_none_or(|rule| !rule.annotation) {
        body["stage"] = json!(e.stage);
    }
    body["updated_at"] = json!(at);
    if body.get("created_at").is_none() {
        body["created_at"] = json!(at);
    }
    let mut block = body["stages"][&e.stage].clone();
    if !block.is_object() {
        block = json!({});
    }
    if let (Some(fields), Some(d)) = (block.as_object_mut(), detail.as_object()) {
        for (k, v) in d {
            fields.insert(k.clone(), v.clone());
        }
        fields.insert("last_event".into(), json!(e.event));
        fields.insert("at".into(), json!(at));
        if let Some(r) = &e.run {
            fields.insert("run".into(), json!(r));
        }
    }
    if !body["stages"].is_object() {
        body["stages"] = json!({});
    }
    body["stages"][&e.stage] = block;
    let mut history = body["history"].as_array().cloned().unwrap_or_default();
    history.push(
        json!({ "at": at, "stage": e.stage, "event": e.event, "run": e.run, "detail": detail }),
    );
    if history.len() > HISTORY_MAX {
        history.drain(0..history.len() - HISTORY_MAX);
    }
    body["history"] = Value::Array(history);
    body
}

/// A guard on recording: the event is recorded only when the item's history
/// holds none of `events` — with `detail`, none whose detail matches it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unless {
    /// `"stage/event"` or `"stage/*"`.
    pub events: Vec<String>,
    /// Every key must equal the matching event's `detail[key]`, compared as text.
    #[serde(default)]
    pub detail: Option<BTreeMap<String, String>>,
}

impl Unless {
    fn check(&self) -> Result<(), StoreError> {
        if self.events.is_empty() || !self.events.iter().all(|key| valid_event_key(key)) {
            return Err(StoreError::Invalid(
                "`unless.events` lists `stage/event` or `stage/*` keys in lowercase ids".into(),
            ));
        }
        Ok(())
    }

    /// The newest event in `body`'s history this guard names.
    pub fn matched<'a>(&self, body: &'a Value) -> Option<&'a Value> {
        history(body).rev().find(|event| {
            let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
            self.events.iter().any(|key| {
                key.split_once('/').is_some_and(|(stage, name)| {
                    stage == text("stage") && (name == "*" || name == text("event"))
                })
            }) && self
                .detail
                .as_ref()
                .is_none_or(|when| detail_matches(when, event.get("detail").unwrap_or(&NO_DETAIL)))
        })
    }
}

/// What recording an event produced.
#[derive(Debug, Clone, Serialize)]
pub struct Recorded {
    pub item: String,
    pub version: i64,
    /// Where the definition puts the item after this event, when a rule places it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A rule makes the event an annotation: noted on the item, which stays put.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub annotation: bool,
    /// Set when the definition has no rule for this `stage/event`: the record
    /// is written, but nothing on a board will move — usually a typo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// The history event an `unless` guard matched: nothing was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<Value>,
}

impl Recorded {
    fn resolved(def: &PipelineDefinition, e: &EventInput, version: i64) -> Self {
        let rule = def.rule_for(&e.stage, &e.event, &e.detail);
        Self {
            item: e.item.clone(),
            version,
            column: rule.and_then(Rule::places).map(str::to_string),
            label: rule.map(|r| def.label_of(r, &e.stage, &e.event, &e.detail)),
            annotation: rule.is_some_and(|r| r.annotation),
            warning: rule.is_none().then(|| no_rule_warning(def, e)),
            matched: None,
        }
    }

    fn skipped(e: &EventInput, version: i64, matched: &Value) -> Self {
        Self {
            item: e.item.clone(),
            version,
            column: None,
            label: None,
            annotation: false,
            warning: None,
            matched: Some(json!({
                "at": matched.get("at"),
                "stage": matched.get("stage"),
                "event": matched.get("event"),
            })),
        }
    }
}

fn check_event(e: &EventInput, unless: Option<&Unless>) -> Result<(), StoreError> {
    crate::store::validate_segment("item", &e.item)?;
    if !valid_id(&e.stage) || !valid_id(&e.event) {
        return Err(StoreError::Invalid(
            "`stage` and `event` are lowercase ids (letters, digits, - or _)".into(),
        ));
    }
    if !(e.detail.is_object() || e.detail.is_string() || e.detail.is_null()) {
        return Err(StoreError::Invalid(
            "`detail` is an object or a one-line string".into(),
        ));
    }
    unless.map_or(Ok(()), Unless::check)
}

/// What an event would do, without writing: the column and label the
/// definition resolves it to, or the event an `unless` guard matches in
/// `current`, the item's record as it stands. A backtest or a dry run records
/// nothing.
pub fn preview_event(
    def: &PipelineDefinition,
    e: &EventInput,
    unless: Option<&Unless>,
    current: &Value,
) -> Result<Recorded, StoreError> {
    check_event(e, unless)?;
    Ok(match unless.and_then(|unless| unless.matched(current)) {
        Some(matched) => Recorded::skipped(e, 0, matched),
        None => Recorded::resolved(def, e, 0),
    })
}

fn no_rule_warning(def: &PipelineDefinition, e: &EventInput) -> String {
    format!(
        "the `{}` pipeline has no rule for `{}/{}`; the record is written but nothing moves on the board — add a rule with pipeline.define or check the spelling",
        def.name, e.stage, e.event
    )
}

/// Record an event against `def`'s collection in `scope`: read the item's
/// record, stand down when `unless` matches it, fold the event in, write back
/// only if nobody else wrote in between, and try again a few times if
/// somebody did — reading the guard again each time.
pub async fn record_event(
    store: &dyn DocumentStore,
    scope: &Scope,
    def: &PipelineDefinition,
    e: &EventInput,
    unless: Option<&Unless>,
) -> Result<Recorded, StoreError> {
    check_event(e, unless)?;
    let rule = def.rule_for(&e.stage, &e.event, &e.detail);
    let at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut last = String::new();
    for _ in 0..RECORD_TRIES {
        let (body, version) = match store.get(scope, &def.collection, &e.item).await? {
            Some(doc) => (doc.body, doc.version),
            None => (json!({}), 0),
        };
        if let Some(matched) = unless.and_then(|unless| unless.matched(&body)) {
            return Ok(Recorded::skipped(e, version, matched));
        }
        match store
            .set(
                scope,
                &def.collection,
                &e.item,
                apply_event(body, e, &at, rule),
                Some(version),
                None,
                e.run.as_deref(),
            )
            .await
        {
            Ok(doc) => return Ok(Recorded::resolved(def, e, doc.version)),
            Err(StoreError::VersionConflict { .. }) => {
                last = "another run wrote the record at the same time".into()
            }
            Err(e) => return Err(e),
        }
    }
    Err(StoreError::Backend(format!(
        "gave up after {RECORD_TRIES} tries: {last}"
    )))
}

// ---------------------------------------------------------------------------
// Definitions in the store
// ---------------------------------------------------------------------------

pub async fn load_definition(
    store: &dyn DocumentStore,
    scope: &Scope,
    id: &str,
) -> Result<Option<(Document, PipelineDefinition)>, StoreError> {
    crate::store::validate_segment("pipeline id", id)?;
    let Some(doc) = store.get(scope, DEFINITIONS, id).await? else {
        return Ok(None);
    };
    let def: PipelineDefinition = serde_json::from_value(doc.body.clone()).map_err(|e| {
        StoreError::Invalid(format!("stored definition `{id}` does not parse: {e}"))
    })?;
    Ok(Some((doc, def)))
}

/// Every definition in `scope` that parses, with its id.
pub async fn list_definitions(
    store: &dyn DocumentStore,
    scope: &Scope,
) -> Result<Vec<(String, PipelineDefinition, Document)>, StoreError> {
    let page = store
        .list(
            scope,
            DEFINITIONS,
            &crate::store::Query {
                limit: Some(200),
                ..Default::default()
            },
        )
        .await?;
    Ok(page
        .documents
        .into_iter()
        .filter_map(|d| {
            serde_json::from_value::<PipelineDefinition>(d.body.clone())
                .ok()
                .map(|def| (d.doc_id.clone(), def, d))
        })
        .collect())
}

/// Create or replace a definition after validating it. `if_version` as in
/// the store; the findings come back with the document so the author sees
/// the warnings.
pub async fn define(
    store: &dyn DocumentStore,
    scope: &Scope,
    id: &str,
    def: &PipelineDefinition,
    if_version: Option<i64>,
    actor: Option<&str>,
) -> Result<(Document, Findings), StoreError> {
    if !valid_id(id) {
        return Err(StoreError::Invalid(format!(
            "pipeline id `{id}` must be lowercase letters, digits, - or _"
        )));
    }
    let findings = def.validate();
    if !findings.ok() {
        return Err(StoreError::Invalid(format!(
            "the definition has problems: {}",
            findings.errors.join("; ")
        )));
    }
    let body = serde_json::to_value(def).map_err(|e| StoreError::Invalid(e.to_string()))?;
    let doc = store
        .set(scope, DEFINITIONS, id, body, if_version, None, actor)
        .await?;
    Ok((doc, findings))
}

/// A stage-by-stage count of a pipeline's items: what a catalogue shows.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub items: usize,
    /// Items per column id (lanes and exits), only columns with any.
    pub columns: BTreeMap<String, usize>,
    /// Items the definition cannot place (no rule for their last event).
    pub unplaced: usize,
    pub last_moved_at: Option<String>,
}

/// An event in an item's history that a rule of the definition places.
#[derive(Debug, Clone, Copy)]
pub struct Placement<'a> {
    pub at: &'a str,
    pub stage: &'a str,
    pub event: &'a str,
    pub run: Option<&'a str>,
    pub detail: &'a Value,
    pub rule: &'a Rule,
    /// The column the rule puts the item on.
    pub column: &'a str,
}

/// Every event of `body`'s history the definition places, in history order.
/// Annotations and events with no rule place nothing.
pub fn placements<'a>(
    def: &'a PipelineDefinition,
    body: &'a Value,
) -> impl Iterator<Item = Placement<'a>> + 'a {
    history(body).filter_map(move |event| {
        let text = |key: &str| event.get(key).and_then(Value::as_str);
        let stage = text("stage").unwrap_or_default();
        let event_name = text("event").unwrap_or_default();
        let detail = event.get("detail").unwrap_or(&NO_DETAIL);
        let rule = def.rule_for(stage, event_name, detail)?;
        Some(Placement {
            column: rule.places()?,
            at: text("at").unwrap_or_default(),
            stage,
            event: event_name,
            run: text("run"),
            detail,
            rule,
        })
    })
}

/// The column an item record is in now, per the definition.
pub fn position_of(def: &PipelineDefinition, body: &Value) -> Option<String> {
    placements(def, body)
        .last()
        .map(|placement| placement.column.to_string())
}

/// `items` grouped by `key`, the groups and their members in order of first
/// appearance.
pub(crate) fn grouped<T, K: PartialEq>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> K,
) -> Vec<(K, Vec<T>)> {
    items
        .into_iter()
        .fold(Vec::new(), |mut groups: Vec<(K, Vec<T>)>, item| {
            let k = key(&item);
            match groups.iter_mut().find(|(group, _)| *group == k) {
                Some((_, members)) => members.push(item),
                None => groups.push((k, vec![item])),
            }
            groups
        })
}

pub fn count(def: &PipelineDefinition, records: &[Document]) -> Counts {
    let mut c = Counts {
        items: records.len(),
        ..Default::default()
    };
    for d in records {
        match position_of(def, &d.body) {
            Some(col) => *c.columns.entry(col).or_default() += 1,
            None => c.unplaced += 1,
        }
        let moved = d
            .body
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_string);
        if moved > c.last_moved_at {
            c.last_moved_at = moved;
        }
    }
    c
}

/// A worked example the tools show the AI: enough to define a pipeline for
/// any workflow by analogy.
pub fn example() -> Value {
    json!({
        "id": "onboarding",
        "definition": {
            "name": "Onboarding",
            "description": "New starters from offer accepted to first login.",
            "guide": "## How it works\n\nA starter enters when HR records the accepted offer. Accounts are created first, then access is granted; a failed grant parks the starter for a person. The welcome email moves them on, and the first login closes the flow.\n\n## What needs a person\n\n- **Needs a person**: an access grant failed.\n- **Offer withdrawn**: nothing to do.",
            "item_label": "starter",
            "collection": "onboarding",
            "lanes": [
                {"id": "accounts", "label": "Creating accounts"},
                {"id": "access", "label": "Granting access"},
                {"id": "welcome", "label": "Welcome sent"},
                {"id": "logged-in", "label": "Logged in"}
            ],
            "exits": [
                {"id": "withdrawn", "label": "Offer withdrawn"},
                {"id": "blocked", "label": "Needs a person"}
            ],
            "events": {
                "accounts/started":  [{"column": "accounts", "tone": "active"}],
                "accounts/done":     [{"column": "access",   "tone": "active"}],
                "access/failed":     [{"column": "blocked",  "tone": "bad"}],
                "access/done":       [{"column": "welcome",  "tone": "good"}],
                "welcome/opened":    [{"column": "logged-in","tone": "good"}],
                "offer/withdrawn":   [{"column": "withdrawn"}]
            },
            "summary_field": "stages.accounts.name",
            "link_fields": [{"field": "hr_url", "label": "HR record"}],
            "sources": ["workflow:new-starter"]
        }
    })
}

/// Keep `Map` in scope for callers building details.
pub type Detail = Map<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::intercepted::{Intercepted, Interceptor};
    use crate::store::MemoryDocumentStore;

    fn orders() -> PipelineDefinition {
        serde_json::from_value(json!({
            "name": "Orders",
            "collection": "pipeline",
            "lanes": [{"id":"implementing","label":"Implementing"},{"id":"reviewing","label":"Reviewing"},{"id":"reviewed","label":"Reviewed"},{"id":"verifying","label":"Verifying"},{"id":"verified","label":"Verified"}],
            "exits": [{"id":"blocked","label":"Review blocked"},{"id":"not-code","label":"Not a code change"}],
            "rungs": ["1h","6h","24h"],
            "events": {
                "implement/claimed": [{"column":"implementing","tone":"active"}],
                "implement/released": [{"when":{"label":"stage-skipped"},"column":"not-code"},{"column":"implementing","tone":"warn","label":"Released"}],
                "review/claimed": [{"column":"reviewing","tone":"active"}],
                "review/finished": [{"when":{"verdict":"stage-review-blocked"},"column":"blocked","tone":"bad"},{"column":"reviewed","tone":"good"}],
                "verify/rung": [{"when":{"state":"pass"},"column":"verifying","tone":"active","rung_field":"rung","rung_passed":true},{"column":"verifying","rung_field":"rung"}],
                "verify/verified": [{"column":"verified","tone":"good","rung_field":"rung","rung_passed":true}]
            },
            "summary_field": "stages.implement.summary"
        }))
        .unwrap()
    }

    #[test]
    fn a_sound_definition_validates_with_no_errors() {
        let f = orders().validate();
        assert!(f.ok(), "{:?}", f.errors);
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    }

    #[test]
    fn validation_names_every_problem() {
        let mut d = orders();
        d.lanes.push(Column {
            id: "Reviewing".into(),
            label: "".into(),
            ..Default::default()
        });
        d.events.insert(
            "merge/gate".into(),
            vec![Rule {
                column: Some("nowhere".into()),
                ..Default::default()
            }],
        );
        d.events.insert("bad key".into(), vec![]);
        d.events.insert(
            "verify/failed".into(),
            vec![
                Rule {
                    column: Some("blocked".into()),
                    ..Default::default()
                },
                Rule {
                    column: Some("blocked".into()),
                    when: Some([("state".to_string(), "regression".to_string())].into()),
                    ..Default::default()
                },
            ],
        );
        d.events.insert(
            "x/y".into(),
            vec![Rule {
                column: Some("verified".into()),
                rung_passed: true,
                ..Default::default()
            }],
        );
        d.exits.push(Column {
            id: "lonely".into(),
            label: "Lonely".into(),
            ..Default::default()
        });
        let f = d.validate();
        let errors = f.errors.join("\n");
        assert!(errors.contains("`Reviewing` must be lowercase"), "{errors}");
        assert!(errors.contains("`Reviewing` needs a label"), "{errors}");
        assert!(errors.contains("`nowhere`, which is neither"), "{errors}");
        assert!(errors.contains("`bad key` must be"), "{errors}");
        assert!(errors.contains("`bad key` has no rules"), "{errors}");
        assert!(
            errors.contains("`rung_passed` without a `rung_field`"),
            "{errors}"
        );
        let warnings = f.warnings.join("\n");
        assert!(
            warnings.contains(
                "`verify/failed` rule 1 has no `when`, so the rules after it can never match"
            ),
            "{warnings}"
        );
        assert!(
            warnings.contains("no event ever lands in exit `lonely`"),
            "{warnings}"
        );
    }

    #[test]
    fn rules_match_on_detail_first_match_wins() {
        let d = orders();
        let r = d
            .rule_for(
                "review",
                "finished",
                &json!({"verdict": "stage-review-blocked"}),
            )
            .unwrap();
        assert_eq!(r.column.as_deref(), Some("blocked"));
        let r = d
            .rule_for("review", "finished", &json!({"verdict": "stage-reviewed"}))
            .unwrap();
        assert_eq!(r.column.as_deref(), Some("reviewed"));
        let r = d.rule_for("review", "finished", &json!({})).unwrap();
        assert_eq!(
            r.column.as_deref(),
            Some("reviewed"),
            "a rule without `when` is the fallback"
        );
        assert!(d.rule_for("merge", "gate", &json!({})).is_none());
        // numbers compare as text
        let mut d2 = d.clone();
        d2.events.insert(
            "x/n".into(),
            vec![Rule {
                when: Some([("n".to_string(), "3".to_string())].into()),
                column: Some("verified".into()),
                ..Default::default()
            }],
        );
        assert!(d2.rule_for("x", "n", &json!({"n": 3})).is_some());
    }

    #[test]
    fn apply_event_builds_the_record_shape() {
        let e = EventInput {
            item: "ORD-1".into(),
            stage: "implement".into(),
            event: "claimed".into(),
            detail: json!({"repo": "org/r"}),
            run: Some("inst-1".into()),
        };
        let body = apply_event(json!({}), &e, "2026-09-12T18:00:00Z", None);
        assert_eq!(body["item"], "ORD-1");
        assert_eq!(body["stage"], "implement");
        assert_eq!(body["created_at"], "2026-09-12T18:00:00Z");
        assert_eq!(body["stages"]["implement"]["repo"], "org/r");
        assert_eq!(body["stages"]["implement"]["run"], "inst-1");
        assert_eq!(body["stages"]["implement"]["last_event"], "claimed");
        assert_eq!(body["history"].as_array().unwrap().len(), 1);
        let e2 = EventInput {
            item: "ORD-1".into(),
            stage: "implement".into(),
            event: "pr-opened".into(),
            detail: json!("https://x/pull/1"),
            run: None,
        };
        let body = apply_event(body, &e2, "2026-09-12T18:05:00Z", None);
        assert_eq!(
            body["created_at"], "2026-09-12T18:00:00Z",
            "creation time is kept"
        );
        assert_eq!(
            body["stages"]["implement"]["repo"], "org/r",
            "earlier keys survive"
        );
        assert_eq!(
            body["stages"]["implement"]["detail"], "https://x/pull/1",
            "a string detail lands under `detail`"
        );
        assert_eq!(body["history"][1]["event"], "pr-opened");
    }

    #[test]
    fn history_is_capped() {
        let mut body = json!({});
        for i in 0..(HISTORY_MAX + 3) {
            let e = EventInput {
                item: "i".into(),
                stage: "s".into(),
                event: format!("e{i}"),
                detail: Value::Null,
                run: None,
            };
            body = apply_event(body, &e, "t", None);
        }
        let h = body["history"].as_array().unwrap();
        assert_eq!(h.len(), HISTORY_MAX);
        assert_eq!(h[0]["event"], "e3");
    }

    #[tokio::test]
    async fn define_record_and_count_round_trip() {
        let store = MemoryDocumentStore::new();
        let scope = Scope::Agent("example".into());
        let (doc, findings) = define(&store, &scope, "orders", &orders(), Some(0), Some("me"))
            .await
            .unwrap();
        assert_eq!(doc.version, 1);
        assert!(findings.ok());
        assert!(matches!(
            define(&store, &scope, "Bad Id", &orders(), None, None)
                .await
                .unwrap_err(),
            StoreError::Invalid(_)
        ));
        let mut broken = orders();
        broken.lanes.clear();
        let err = define(&store, &scope, "orders", &broken, None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("`lanes` needs at least one lane"),
            "{err}"
        );

        let (_, def) = load_definition(&store, &scope, "orders")
            .await
            .unwrap()
            .unwrap();
        let r = record_event(
            &store,
            &scope,
            &def,
            &EventInput {
                item: "ORD-1".into(),
                stage: "implement".into(),
                event: "claimed".into(),
                detail: json!({"summary": "s"}),
                run: Some("r1".into()),
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (r.version, r.column.as_deref(), r.label.as_deref()),
            (1, Some("implementing"), Some("Implementing"))
        );
        assert!(r.warning.is_none());
        let r = record_event(
            &store,
            &scope,
            &def,
            &EventInput {
                item: "ORD-1".into(),
                stage: "review".into(),
                event: "finished".into(),
                detail: json!({"verdict": "stage-review-blocked"}),
                run: None,
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!((r.version, r.column.as_deref()), (2, Some("blocked")));
        let r = record_event(
            &store,
            &scope,
            &def,
            &EventInput {
                item: "ORD-2".into(),
                stage: "merge".into(),
                event: "gate".into(),
                detail: Value::Null,
                run: None,
            },
            None,
        )
        .await
        .unwrap();
        assert!(r.warning.unwrap().contains("no rule for `merge/gate`"));
        assert!(matches!(
            record_event(
                &store,
                &scope,
                &def,
                &EventInput {
                    item: "ORD-3".into(),
                    stage: "Bad Stage".into(),
                    event: "x".into(),
                    detail: Value::Null,
                    run: None
                },
                None
            )
            .await
            .unwrap_err(),
            StoreError::Invalid(_)
        ));

        let items = store
            .list(&scope, "pipeline", &crate::store::Query::default())
            .await
            .unwrap()
            .documents;
        let counts = count(&def, &items);
        assert_eq!(counts.items, 2);
        assert_eq!(counts.columns.get("blocked"), Some(&1));
        assert_eq!(counts.unplaced, 1);
        assert_eq!(list_definitions(&store, &scope).await.unwrap().len(), 1);
    }

    #[test]
    fn a_guide_is_kept_and_bounded() {
        let mut d = orders();
        d.guide = Some("## How it works\n\nTickets go left to right.".into());
        assert!(d.validate().ok());
        let round_trip: PipelineDefinition =
            serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(round_trip.guide, d.guide);

        d.guide = Some("x".repeat(GUIDE_MAX_BYTES + 1));
        let f = d.validate();
        assert_eq!(f.errors.len(), 1, "{:?}", f.errors);
        assert!(
            f.errors[0].starts_with("`guide` is 65537 bytes; the limit is 65536"),
            "{}",
            f.errors[0]
        );

        let mut none = orders();
        none.guide = None;
        assert!(!serde_json::to_value(&none)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("guide"));
    }

    #[test]
    fn a_healing_tone_parses_and_round_trips() {
        let rule: Rule = serde_json::from_value(json!({
            "column": "verifying",
            "tone": "healing",
            "label": "Inconclusive · self-healing"
        }))
        .unwrap();
        assert_eq!(rule.tone, Some(Tone::Healing));
        let v = serde_json::to_value(&rule).unwrap();
        assert_eq!(v["tone"], "healing");
        let back: Rule = serde_json::from_value(v).unwrap();
        assert_eq!(back, rule);

        let mut d = orders();
        d.events.insert("verify/inconclusive".into(), vec![rule]);
        let f = d.validate();
        assert!(f.ok(), "{:?}", f.errors);
        let round_trip: PipelineDefinition =
            serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert_eq!(round_trip, d);
    }

    fn timings(milestones: &[&str], ends: &[&str]) -> Timings {
        Timings {
            origin: Some(Origin {
                field: "filed_at".into(),
                label: "Filed".into(),
            }),
            milestones: Some(milestones.iter().map(|id| id.to_string()).collect()),
            ends: Some(ends.iter().map(|id| id.to_string()).collect()),
        }
    }

    #[test]
    fn a_definition_with_timings_claims_and_due_times_validates_with_no_warnings() {
        let mut d = orders();
        d.timings = Some(timings(
            &["reviewed", "verified"],
            &["verified", "not-code"],
        ));
        let claimed = d.events.get_mut("implement/claimed").unwrap();
        claimed[0].claim = true;
        claimed[0].lease_field = Some("lease_minutes".into());
        d.events.get_mut("verify/rung").unwrap()[1].due_field = Some("due_at".into());
        let f = d.validate();
        assert!(f.ok(), "{:?}", f.errors);
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    }

    #[test]
    fn timing_and_rule_field_problems_are_named() {
        let mut d = orders();
        d.timings = Some(Timings {
            origin: Some(Origin {
                field: "filed at".into(),
                label: " ".into(),
            }),
            ..timings(
                &["blocked", "nowhere", "verified", "verified"],
                &["gone", "not-code", "not-code"],
            )
        });
        d.events.insert(
            "x/lease".into(),
            vec![Rule {
                column: Some("verifying".into()),
                lease_field: Some("lease_minutes".into()),
                ..Default::default()
            }],
        );
        d.events.insert(
            "x/both".into(),
            vec![Rule {
                column: Some("verifying".into()),
                claim: true,
                due_field: Some("due/at".into()),
                ..Default::default()
            }],
        );
        let errors = d.validate().errors.join("\n");
        for expected in [
            "`timings.origin.field` `filed at` may only contain",
            "`timings.origin` needs a `label`",
            "`timings.milestones` names `blocked`, which is an exit; milestones are lanes",
            "`timings.milestones` names `nowhere`, which is not a lane",
            "`timings.milestones` names `verified` more than once",
            "`timings.ends` names `gone`, which is neither a lane nor an exit",
            "`timings.ends` names `not-code` more than once",
            "`x/lease` rule 1 sets `lease_field` without `claim`",
            "`x/both` rule 1 sets both `claim` and `due_field`",
            "`x/both` rule 1 `due_field` `due/at` may only contain",
        ] {
            assert!(
                errors.contains(expected),
                "missing {expected:?} in\n{errors}"
            );
        }
    }

    #[test]
    fn an_empty_milestones_list_warns() {
        let mut d = orders();
        d.timings = Some(timings(&[], &["verified"]));
        let f = d.validate();
        assert!(f.ok(), "{:?}", f.errors);
        assert_eq!(
            f.warnings,
            ["an empty `timings.milestones` reports none; omit it for the last lane"]
        );
        assert!(d.milestones().is_empty());
    }

    #[test]
    fn the_new_fields_serialise_only_when_set_and_revision_9_round_trips() {
        let shipped: Value = serde_json::from_str(fixtures::ORDERS_R9).unwrap();
        let def = fixtures::orders();
        assert_eq!(serde_json::to_value(&def).unwrap(), shipped);

        let mut timed = def.clone();
        timed.timings = Some(Timings {
            origin: None,
            milestones: Some(vec!["deployed".into()]),
            ends: None,
        });
        timed.events.get_mut("verify/started").unwrap()[0] = Rule {
            claim: true,
            lease_field: Some("lease_minutes".into()),
            ..def.events["verify/started"][0].clone()
        };
        let v = serde_json::to_value(&timed).unwrap();
        assert_eq!(v["timings"], json!({"milestones": ["deployed"]}));
        assert_eq!(v["events"]["verify/started"][0]["claim"], true);
        assert!(v["events"]["verify/rung"][0].get("claim").is_none());
        assert!(v["events"]["verify/rung"][0].get("due_field").is_none());
        let back: PipelineDefinition = serde_json::from_value(v).unwrap();
        assert_eq!(back, timed);
    }

    #[test]
    fn without_timings_the_last_lane_is_the_milestone_and_the_end() {
        let d = fixtures::orders();
        assert!(d.timings.is_none());
        let milestones: Vec<&str> = d.milestones().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(milestones, ["verified"]);
        assert_eq!(d.ends(), ["verified"]);
        assert_eq!(d.lane_index("deployed"), Some(6));
        assert_eq!(d.lane_index("not-code"), None);

        let mut timed = d.clone();
        timed.timings = Some(timings(
            &["verified", "deployed"],
            &["duplicate", "verified"],
        ));
        let milestones: Vec<&str> = timed.milestones().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            milestones,
            ["deployed", "verified"],
            "milestones follow lane order"
        );
        assert_eq!(timed.ends(), ["duplicate", "verified"]);
    }

    #[test]
    fn position_of_is_the_last_placement() {
        let def = fixtures::orders();
        let item = fixtures::ord_2797();
        let placed: Vec<(&str, &str)> = placements(&def, &item.body)
            .map(|p| (p.event, p.column))
            .collect();
        assert_eq!(placed.len(), 15, "`review/follow-ups` has no rule");
        assert_eq!(placed[9], ("gate", "human-merge"));
        assert_eq!(
            position_of(&def, &item.body),
            placements(&def, &item.body)
                .last()
                .map(|p| p.column.to_string())
        );
        assert_eq!(position_of(&def, &item.body).as_deref(), Some("verifying"));
        assert_eq!(
            placements(&def, &item.body).last().map(|p| (p.at, p.run)),
            Some(("2026-09-15T04:53:20.286220Z", Some("0e3be51e")))
        );
    }

    #[test]
    fn clip_cuts_at_characters_and_marks_the_cut() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip("a bcd", 3), "a…");
        assert_eq!(clip("éééé", 3), "éé…");
        assert_eq!(clip("", 3), "");
    }

    fn annotated() -> PipelineDefinition {
        let mut def = fixtures::orders();
        def.events.insert(
            "review/overlap-judged".into(),
            vec![
                Rule {
                    when: Some([("relation".to_string(), "conflicting".to_string())].into()),
                    annotation: true,
                    tone: Some(Tone::Warn),
                    label_field: Some("headline".into()),
                    ..Default::default()
                },
                Rule {
                    annotation: true,
                    tone: Some(Tone::Neutral),
                    label: Some("overlap judged".into()),
                    label_field: Some("headline".into()),
                    ..Default::default()
                },
            ],
        );
        def
    }

    #[test]
    fn annotation_rules_validate_and_every_misuse_is_named() {
        let def = annotated();
        let f = def.validate();
        assert!(f.ok(), "{:?}", f.errors);

        let mut misused = def.clone();
        misused.events.insert(
            "watch/escalated".into(),
            vec![Rule {
                annotation: true,
                column: Some("verifying".into()),
                claim: true,
                lease_field: Some("lease_minutes".into()),
                due_field: Some("due_at".into()),
                rung_field: Some("rung".into()),
                rung_passed: true,
                label_field: Some("bad field".into()),
                ..Default::default()
            }],
        );
        misused.events.insert(
            "watch/stale".into(),
            vec![Rule {
                tone: Some(Tone::Neutral),
                ..Default::default()
            }],
        );
        let errors = misused.validate().errors.join("\n");
        for field in [
            "column",
            "claim",
            "lease_field",
            "due_field",
            "rung_field",
            "rung_passed",
        ] {
            assert!(
                errors.contains(&format!("`watch/escalated` rule 1 is an annotation, which never moves the item, so it may not set `{field}`")),
                "{field}: {errors}"
            );
        }
        assert!(
            errors.contains("`watch/escalated` rule 1 `label_field` `bad field` may only contain"),
            "{errors}"
        );
        assert!(
            errors.contains("`watch/stale` rule 1 needs a `column`, or `annotation: true`"),
            "{errors}"
        );
        assert!(
            !errors.contains("without `claim`"),
            "an annotation is not also checked as a placing rule: {errors}"
        );

        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(
            v["events"]["review/overlap-judged"][1],
            json!({"annotation": true, "tone": "neutral", "label": "overlap judged", "label_field": "headline"})
        );
        assert!(v["events"]["implement/claimed"][0]
            .get("annotation")
            .is_none());
        let back: PipelineDefinition = serde_json::from_value(v).unwrap();
        assert_eq!(back, def);

        let mut clashing = fixtures::orders();
        clashing.collection = notices::COLLECTION.into();
        assert!(clashing
            .validate()
            .errors
            .join("\n")
            .contains("that collection holds pipeline notices"));
    }

    fn event(item: &str, stage: &str, name: &str, detail: Value) -> EventInput {
        EventInput {
            item: item.into(),
            stage: stage.into(),
            event: name.into(),
            detail,
            run: Some("r1".into()),
        }
    }

    #[tokio::test]
    async fn an_annotation_is_noted_without_moving_the_item_or_its_stage() {
        let store = MemoryDocumentStore::new();
        let scope = Scope::Agent("example".into());
        let def = annotated();
        record_event(
            &store,
            &scope,
            &def,
            &event("ORD-1", "implement", "claimed", json!({})),
            None,
        )
        .await
        .unwrap();
        let judged = record_event(
            &store,
            &scope,
            &def,
            &event(
                "ORD-1",
                "review",
                "overlap-judged",
                json!({"relation": "conflicting", "headline": "  #1698 and #1702\nboth change <validateShiftState> "}),
            ),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                judged.column.as_deref(),
                judged.label.as_deref(),
                judged.annotation,
                &judged.warning
            ),
            (
                None,
                Some("#1698 and #1702 both change validateShiftState"),
                true,
                &None
            )
        );

        let body = store
            .get(&scope, &def.collection, "ORD-1")
            .await
            .unwrap()
            .unwrap()
            .body;
        assert_eq!(body["stage"], "implement");
        assert_eq!(body["stages"]["review"]["relation"], "conflicting");
        assert_eq!(body["history"].as_array().unwrap().len(), 2);
        assert_eq!(position_of(&def, &body).as_deref(), Some("implementing"));
        let counts = count(&def, &[fixtures::doc("ORD-1", body)]);
        assert_eq!(
            (counts.columns.get("implementing"), counts.unplaced),
            (Some(&1), 0)
        );

        let preview = preview_event(
            &def,
            &event(
                "ORD-1",
                "review",
                "overlap-judged",
                json!({"relation": "complementary"}),
            ),
            None,
            &Value::Null,
        )
        .unwrap();
        assert_eq!(
            (preview.label.as_deref(), preview.annotation, preview.column),
            (Some("overlap judged"), true, None)
        );
    }

    #[test]
    fn label_field_text_replaces_the_label_and_falls_back_when_empty() {
        let def = annotated();
        let rule = &def.events["review/overlap-judged"][1];
        let label = |detail: Value| def.label_of(rule, "review", "overlap-judged", &detail);
        assert_eq!(
            label(json!({"headline": "merged by octocat"})),
            "merged by octocat"
        );
        for empty in [
            json!({}),
            json!({"headline": "  \n "}),
            json!({"headline": 42}),
            Value::Null,
        ] {
            assert_eq!(label(empty), "overlap judged");
        }
        let unlabelled = Rule {
            label: None,
            ..rule.clone()
        };
        assert_eq!(
            def.label_of(&unlabelled, "review", "overlap-judged", &json!({})),
            "review/overlap-judged"
        );
        let placing = Rule {
            column: Some("merging".into()),
            label_field: Some("note".into()),
            ..Default::default()
        };
        assert_eq!(
            def.label_of(&placing, "merge", "merged", &json!({"note": ""})),
            def.column_label("merging")
        );
        assert_eq!(
            def.label_of(
                &placing,
                "merge",
                "merged",
                &json!({"note": "merged by octocat"})
            ),
            "merged by octocat"
        );
    }

    #[test]
    fn label_text_reads_a_detail_as_the_board_does() {
        let x = |n: usize| "x".repeat(n);
        let words = "word ".repeat(30);
        for (value, expected) in [
            (
                json!("  merged by octocat,\n waiting for the deploy  "),
                Some("merged by octocat, waiting for the deploy".to_string()),
            ),
            (
                json!("<!channel> ping <@U123>"),
                Some("!channel ping @U123".into()),
            ),
            (json!("tab\tand\u{7}bell"), Some("tab and bell".into())),
            (json!(""), None),
            (json!("   "), None),
            (json!(42), None),
            (json!(x(120)), Some(x(120))),
            (json!(x(121)), Some(format!("{}…", x(119)))),
            (
                json!("é".repeat(130)),
                Some(format!("{}…", "é".repeat(119))),
            ),
            (
                json!(words.clone()),
                Some(format!(
                    "{}…",
                    words.chars().take(119).collect::<String>().trim_end()
                )),
            ),
        ] {
            assert_eq!(label_text(&value), expected, "{value}");
        }
    }

    const PR: &str = "https://github.com/acme/app/pull/1702";

    fn unless_merged_or_deploying(pr_url: &str) -> Unless {
        Unless {
            events: vec!["merge/auto-merged".into(), "deploy/*".into()],
            detail: Some([("pr_url".to_string(), pr_url.to_string())].into()),
        }
    }

    fn with_pr(stage: &str, name: &str) -> EventInput {
        event("ORD-2817", stage, name, json!({"pr_url": PR}))
    }

    #[tokio::test]
    async fn unless_skips_on_a_matching_event_and_detail_and_records_otherwise() {
        let store = MemoryDocumentStore::new();
        let scope = Scope::Agent("example".into());
        let def = fixtures::orders();
        record_event(&store, &scope, &def, &with_pr("deploy", "claimed"), None)
            .await
            .unwrap();

        let merged = with_pr("merge", "auto-merged");
        let skipped = record_event(
            &store,
            &scope,
            &def,
            &merged,
            Some(&unless_merged_or_deploying(PR)),
        )
        .await
        .unwrap();
        assert_eq!(skipped.version, 1);
        assert_eq!(
            skipped
                .matched
                .as_ref()
                .map(|m| (m["stage"].clone(), m["event"].clone())),
            Some((json!("deploy"), json!("claimed")))
        );
        assert_eq!(
            (skipped.column, skipped.label, skipped.warning),
            (None, None, None)
        );
        let body = store
            .get(&scope, &def.collection, "ORD-2817")
            .await
            .unwrap()
            .unwrap()
            .body;
        assert_eq!(
            body["history"].as_array().unwrap().len(),
            1,
            "a skipped event writes nothing"
        );
        assert!(
            preview_event(&def, &merged, Some(&unless_merged_or_deploying(PR)), &body)
                .unwrap()
                .matched
                .is_some()
        );

        let another = "https://github.com/acme/app/pull/1698";
        let recorded = record_event(
            &store,
            &scope,
            &def,
            &merged,
            Some(&unless_merged_or_deploying(another)),
        )
        .await
        .unwrap();
        assert_eq!((recorded.version, recorded.matched), (2, None));
        let by_event = |events: &[&str]| Unless {
            events: events.iter().map(|key| key.to_string()).collect(),
            detail: None,
        };
        let merges = record_event(&store, &scope, &def, &merged, Some(&by_event(&["merge/*"])))
            .await
            .unwrap();
        assert_eq!(
            merges.matched.map(|m| m["event"].clone()),
            Some(json!("auto-merged"))
        );
        assert_eq!(
            record_event(
                &store,
                &scope,
                &def,
                &merged,
                Some(&by_event(&["verify/started"]))
            )
            .await
            .unwrap()
            .version,
            3
        );

        for events in [&[][..], &["Deploy/claimed"], &["deploy"]] {
            let err = record_event(&store, &scope, &def, &merged, Some(&by_event(events)))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("`unless.events`"), "{err}");
        }
    }

    /// A store runs race on: its first `readers` reads wait for one another,
    /// so each of those runs reads the record before any of them writes, and
    /// another run records `racer`, when set, just before the first write
    /// lands.
    type RacingStore = Intercepted<MemoryDocumentStore, Race>;

    struct Race {
        racer: parking_lot::Mutex<Option<EventInput>>,
        readers: usize,
        reads: std::sync::atomic::AtomicUsize,
        together: tokio::sync::Barrier,
    }

    impl RacingStore {
        fn new(readers: usize, racer: Option<EventInput>) -> Self {
            Intercepted {
                inner: MemoryDocumentStore::new(),
                interceptor: Race {
                    racer: parking_lot::Mutex::new(racer),
                    readers,
                    reads: Default::default(),
                    together: tokio::sync::Barrier::new(readers),
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl Interceptor for Race {
        async fn before_get(&self, _inner: &dyn DocumentStore) {
            if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < self.readers {
                self.together.wait().await;
            }
        }

        async fn before_set(
            &self,
            inner: &dyn DocumentStore,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
        ) -> Result<(), StoreError> {
            let racer = self.racer.lock().take();
            if let Some(racer) = racer {
                let raced = apply_event(json!({}), &racer, "2026-09-15T05:10:53Z", None);
                inner
                    .set(scope, collection, doc_id, raced, Some(0), None, None)
                    .await?;
            }
            Ok(())
        }
    }

    /// Six release sweeps record the same merge at once on an item `seed`
    /// has put on the board, each behind an `unless` naming the merge: one
    /// event lands and the rest stand down.
    async fn six_sweeps_record_the_merge_once(store: &dyn DocumentStore, seed: &dyn DocumentStore) {
        let scope = Scope::Agent("example".into());
        let def = fixtures::orders();
        record_event(seed, &scope, &def, &with_pr("merge", "gate"), None)
            .await
            .unwrap();
        let unless = Unless {
            events: vec!["merge/merged".into()],
            detail: Some([("pr_url".to_string(), PR.to_string())].into()),
        };
        let sweeps = (0..6).map(|n| {
            let merged = EventInput {
                run: Some(format!("order-release-{n}")),
                ..with_pr("merge", "merged")
            };
            let (scope, def, unless) = (&scope, &def, &unless);
            async move {
                record_event(store, scope, def, &merged, Some(unless))
                    .await
                    .unwrap()
            }
        });
        let outcomes = futures::future::join_all(sweeps).await;
        assert_eq!(
            outcomes.iter().filter(|o| o.matched.is_none()).count(),
            1,
            "{outcomes:?}"
        );
        let body = store
            .get(&scope, &def.collection, "ORD-2817")
            .await
            .unwrap()
            .unwrap()
            .body;
        assert_eq!(history(&body).filter(|e| e["event"] == "merged").count(), 1);
    }

    #[tokio::test]
    async fn six_concurrent_sweeps_behind_one_unless_record_the_merge_once() {
        let racing = RacingStore::new(6, None);
        six_sweeps_record_the_merge_once(&racing, &racing.inner).await;
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn six_concurrent_sweeps_on_postgres_record_the_merge_once() {
        workforce_postgres::test_support::with_test_pool(
            crate::store::postgres::build_pool,
            |pool| async move {
                crate::store::postgres::run_migrations(&pool).await.unwrap();
                let store = crate::store::postgres::PostgresDocumentStore::new(pool);
                six_sweeps_record_the_merge_once(&store, &store).await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn unless_is_read_again_after_a_concurrent_write() {
        let store = RacingStore::new(1, Some(with_pr("deploy", "claimed")));
        let scope = Scope::Agent("example".into());
        let def = fixtures::orders();
        let recorded = record_event(
            &store,
            &scope,
            &def,
            &with_pr("merge", "auto-merged"),
            Some(&unless_merged_or_deploying(PR)),
        )
        .await
        .unwrap();
        assert_eq!(
            recorded.matched.map(|m| m["event"].clone()),
            Some(json!("claimed"))
        );
        let body = store
            .inner
            .get(&scope, &def.collection, "ORD-2817")
            .await
            .unwrap()
            .unwrap()
            .body;
        let events: Vec<&str> = history(&body)
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            events,
            ["claimed"],
            "the retry read the concurrent claim and wrote nothing"
        );
    }

    #[test]
    fn revision_11_with_annotations_label_fields_merges_and_windows_validates_with_no_errors() {
        let shipped: Value = serde_json::from_str(fixtures::ORDERS_R11).unwrap();
        let def = fixtures::orders_r11();
        let f = def.validate();
        assert!(f.ok(), "{:?}", f.errors);
        assert_eq!(serde_json::to_value(&def).unwrap(), shipped);
        let windowed: Vec<(&str, Window)> = def
            .lanes
            .iter()
            .chain(&def.exits)
            .filter_map(|column| Some((column.id.as_str(), column.window?)))
            .collect();
        assert_eq!(
            windowed,
            [
                ("verified", Window::Days(7)),
                ("not-code", Window::Days(7)),
                ("duplicate", Window::Days(7)),
                ("superseded", Window::Days(7)),
            ]
        );
        assert_eq!(def.cards(), DEFAULT_CARDS);

        let item = fixtures::ord_2797();
        let placed: Vec<&str> = placements(&def, &item.body).map(|p| p.event).collect();
        assert_eq!(
            placed.len(),
            15,
            "`review/follow-ups` is now an annotation and still places nothing"
        );
    }

    #[test]
    fn a_tone_displays_as_it_serialises() {
        for tone in [
            Tone::Active,
            Tone::Good,
            Tone::Warn,
            Tone::Healing,
            Tone::Bad,
            Tone::Neutral,
        ] {
            assert_eq!(json!(tone.to_string()), json!(tone));
        }
    }

    #[test]
    fn a_window_keeps_the_spelling_it_was_written_in() {
        for (text, window, hours) in [
            ("24h", Window::Hours(24), Some(24)),
            ("36h", Window::Hours(36), Some(36)),
            ("7d", Window::Days(7), Some(168)),
            ("168h", Window::Hours(168), Some(168)),
            ("30d", Window::Days(30), Some(720)),
            ("3650d", Window::Days(3650), Some(87_600)),
            ("all", Window::All, None),
        ] {
            assert_eq!(text.parse::<Window>(), Ok(window), "{text}");
            assert_eq!(serde_json::to_value(window).unwrap(), json!(text));
            assert_eq!(
                serde_json::from_value::<Window>(json!(text)).unwrap(),
                window
            );
            assert_eq!(
                window.duration().map(|span| span.num_hours()),
                hours,
                "{text}"
            );
        }
    }

    #[test]
    fn a_window_refuses_each_misspelling_with_its_own_message() {
        for (text, message) in [
            ("0d", "`0d` is shorter than 24 hours"),
            ("23h", "`23h` is shorter than 24 hours"),
            ("7", "`7` needs a unit: `h` for hours or `d` for days"),
            ("7w", "`7w` is not `<n>h`, `<n>d` or `all`"),
            ("1.5d", "`1.5d` needs a whole number before `d`"),
            ("-1d", "`-1d` needs a whole number before `d`"),
            ("07d", "`07d` starts with a zero"),
            ("3651d", "`3651d` is longer than 3650 days"),
            ("99999999999h", "`99999999999h` is longer than 3650 days"),
            (
                "",
                "a window may not be empty; write `<n>h`, `<n>d` or `all`",
            ),
        ] {
            assert_eq!(text.parse::<Window>(), Err(message.to_string()), "{text}");
        }
        let refused = serde_json::from_value::<Column>(
            json!({"id": "verified", "label": "Verified", "window": "7w"}),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("`7w` is not"), "{refused}");
    }

    /// The test pipeline with its clock stopping at `ends` (the last lane when
    /// empty) and a week's window on each of `windowed`.
    fn windowed(ends: &[&str], windowed: &[&str]) -> PipelineDefinition {
        let mut def = orders();
        def.timings = (!ends.is_empty()).then(|| Timings {
            ends: Some(ends.iter().map(|id| id.to_string()).collect()),
            ..Default::default()
        });
        for column in def.lanes.iter_mut().chain(&mut def.exits) {
            if windowed.contains(&column.id.as_str()) {
                column.window = Some(Window::Days(7));
            }
        }
        def
    }

    #[test]
    fn board_cards_are_between_1_and_50_and_default_to_10() {
        for (cards, refused) in [(0, true), (1, false), (50, false), (51, true)] {
            let mut def = orders();
            def.board = Some(Board { cards: Some(cards) });
            let errors = def.validate().errors;
            let expected: Vec<String> = refused
                .then(|| format!("`board.cards` is {cards}; it must be between 1 and 50"))
                .into_iter()
                .collect();
            assert_eq!(errors, expected, "{cards}");
            assert_eq!(def.cards(), cards);
        }
        assert_eq!(orders().cards(), DEFAULT_CARDS);
        let without_board = serde_json::to_value(orders()).unwrap();
        assert!(without_board.get("board").is_none());
        assert!(without_board["lanes"][0].get("window").is_none());
    }

    #[test]
    fn a_window_is_allowed_only_where_the_clock_stops() {
        let errors = |def: PipelineDefinition| def.validate().errors;
        assert!(errors(windowed(&[], &["verified"])).is_empty());
        assert!(errors(windowed(
            &["verified", "not-code"],
            &["verified", "not-code"]
        ))
        .is_empty());
        assert_eq!(
            errors(windowed(&["not-code"], &["verifying", "verified"])),
            [
                "lane `verifying` sets `window`, but an item's clock does not stop there (`timings.ends`); only finished items may be hidden",
                "lane `verified` sets `window`, but an item's clock does not stop there (`timings.ends`); only finished items may be hidden",
            ]
        );
        assert_eq!(
            errors(windowed(&[], &["not-code"])),
            ["exit `not-code` sets `window`, but an item's clock does not stop there (`timings.ends`); only finished items may be hidden"]
        );
        assert_eq!(
            errors(windowed(&["verified"], &["not-code"])),
            ["exit `not-code` sets `window`, but an item's clock does not stop there (`timings.ends`); only finished items may be hidden"]
        );
    }

    #[test]
    fn a_rule_that_needs_a_person_may_not_land_in_a_windowed_column() {
        let mut def = windowed(
            &["verified", "blocked", "not-code"],
            &["verified", "blocked", "not-code"],
        );
        def.events.insert(
            "verify/stuck".into(),
            vec![
                Rule {
                    when: Some([("state".to_string(), "stuck".to_string())].into()),
                    column: Some("verified".into()),
                    tone: Some(Tone::Warn),
                    ..Default::default()
                },
                Rule {
                    column: Some("verified".into()),
                    tone: Some(Tone::Good),
                    ..Default::default()
                },
            ],
        );
        def.events.insert(
            "verify/noted".into(),
            vec![Rule {
                annotation: true,
                tone: Some(Tone::Bad),
                ..Default::default()
            }],
        );
        assert_eq!(
            def.validate().errors,
            [
                "`review/finished` rule 1 sends the item to `blocked` with tone `bad`, but `blocked` sets `window`; an item that needs a person must never be hidden",
                "`verify/stuck` rule 1 sends the item to `verified` with tone `warn`, but `verified` sets `window`; an item that needs a person must never be hidden",
            ]
        );
    }

    #[test]
    fn the_example_validates() {
        let v = example();
        let def: PipelineDefinition = serde_json::from_value(v["definition"].clone()).unwrap();
        let f = def.validate();
        assert!(f.ok(), "{:?}", f.errors);
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    }
}
