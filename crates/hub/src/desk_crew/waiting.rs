//! No session waits on its boss in silence (Cufflink Migration, 19 Nov:
//! it laid out options, said "both need your decision", and sat stopped
//! for six minutes while nothing reached it and nobody told the person).
//!
//! Every minute, each live session a boss runs or oversees that stopped
//! with the ball in its boss's court is looked at: its newest word to the
//! boss is newer than anything the boss got to it, and it lays out a
//! choice, asks, or says it holds; or the boss's newest word to it holds it
//! for an answer. Within [`Wake::waiting_after_minutes`] the boss answers
//! (its answer reaching the session ends it), or the person is given the
//! choice as an open item: what waits, its options, the recommendation,
//! that it is theirs to decide, answered from its buttons straight to the
//! session. A boss's hold says it won't answer now: the person is given it
//! at once. A question asked on the line (an ask) has its own way to the
//! person, escalated or lapsed, and isn't looked at here.
//!
//! - **Reminded until answered**, on `waiting_remind_minutes` (10, 30, then
//!   hourly by default), each saying how long it has waited; never in the
//!   person's quiet hours or while they are away, and never parked.
//! - **Settled elsewhere, closed and said in one line**: the person's own
//!   input into the session, the boss's word reaching it, or the session
//!   going on by itself closes the item and stops the reminders.
//!
//! [`Wake::waiting_after_minutes`]: workforce_common::oversight::Wake

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_common::oversight::Oversight;
use workforce_desk::crew::{self, PartyKind};
use workforce_desk::line::{Delivery, LineKind, LineMessage};
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::open_item::{ClosedBy, NewItem, OpenItem, OpenKind};
use workforce_desk::open_item_store::Opened;

use super::CrewHub;

/// What a waiting item's key starts with: `waiting:{session}:{message}`.
pub(crate) const KEY: &str = "waiting:";

/// How far back a session's line is read for who spoke last.
const LOOK_BACK: i64 = 30;

/// The most characters of the session's words an item keeps.
const KEPT_CHARS: usize = 1_500;

/// Words a session says when what it says waits on its boss's answer.
const WAITS: [&str; 12] = [
    "need your decision",
    "needs your decision",
    "your call",
    "your decision",
    "which do you want",
    "which would you like",
    "let me know which",
    "let me know how",
    "waiting on your",
    "waiting for your",
    "until you decide",
    "until desk",
];

/// Whether `key` is a waiting item's.
pub(crate) fn is_waiting(key: &str) -> bool {
    key.starts_with(KEY)
}

/// The key of session `s` waiting on its boss since message `m` (the
/// session's word, or the boss's hold).
fn key(s: Uuid, m: Uuid) -> String {
    format!("{KEY}{s}:{m}")
}

/// The session a waiting key is about.
fn session_of(key: &str) -> Option<Uuid> {
    key.strip_prefix(KEY)?.split(':').next()?.parse().ok()
}

/// Whether `phrase` is in `lower` as words: never "your callback" for
/// "your call", nor "until desktop" for "until desk".
fn says(lower: &str, phrase: &str) -> bool {
    lower.match_indices(phrase).any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether a session's own words to its boss wait on its answer: a
/// question, "both need your decision", "I change nothing until Desk
/// answers", or options laid out with a recommendation. A report that
/// only lists what it did ("1. Updated the migration", "CI: green") never
/// does.
pub(crate) fn waits_on_answer(text: &str) -> bool {
    let lower = text.to_lowercase();
    workforce_desk::line::holds(text)
        || WAITS.iter().any(|w| says(&lower, w))
        || crew::question_in(text).is_some()
        || (options_in(text).len() >= 2 && says(&lower, "recommend"))
}

/// The options a message lays out, one per line, each marked by a single
/// letter: "(a) …", "a) …", "Option A: …". Numbered steps and labels
/// ("1. Ran the tests", "CI: green") are no options.
pub(crate) fn options_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        let mut chars = line.char_indices();
        let marked = match (chars.next(), chars.next(), chars.next()) {
            // "(a) …"
            (Some((_, '(')), Some((_, c)), Some((at, ')'))) if c.is_ascii_alphabetic() => {
                Some(&line[at + 1..])
            }
            // "a) …", "a. …"
            (Some((_, c)), Some((at, ')' | '.')), _) if c.is_ascii_alphabetic() => {
                Some(&line[at + 1..])
            }
            _ => line
                .get(..7)
                .filter(|p| p.eq_ignore_ascii_case("option "))
                .and_then(|_| {
                    let rest = &line[7..];
                    let mut r = rest.char_indices();
                    match (r.next(), r.next()) {
                        (Some((_, c)), Some((at, ':' | ')' | '.' | ' ')))
                            if c.is_ascii_alphanumeric() =>
                        {
                            Some(&rest[at + 1..])
                        }
                        _ => None,
                    }
                }),
        };
        if let Some(o) = marked
            .map(|o| o.trim_start_matches([':', ' ', '-']).trim())
            .filter(|o| o.split_whitespace().count() >= 2)
        {
            out.push(crew::clip(o, workforce_desk::open_item::POINT_CHARS));
        }
    }
    out.truncate(workforce_desk::open_item::POINTS);
    out
}

/// What a message recommends, when it says: its sentence with "recommend".
fn recommendation_in(text: &str) -> Option<String> {
    text.split(['\n', '.'])
        .map(str::trim)
        .find(|s| s.to_lowercase().contains("recommend"))
        .map(|s| crew::clip(s, 300))
}

/// The cadence of reminders: minutes after it was said, then after each
/// reminder; the last one repeats.
pub(crate) fn remind_after(cadence: &[u32], reminded: i32) -> Option<u32> {
    let gaps: Vec<u32> = cadence.iter().copied().filter(|m| *m > 0).collect();
    let n = usize::try_from(reminded).unwrap_or(0);
    gaps.get(n).or(gaps.last()).copied()
}

/// How long `since` is from `now`, as the person reads it.
fn waited(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let m = (now - since).num_minutes().max(1);
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// What waits on the boss, of one session: since when, and the message.
struct Waiting {
    since: DateTime<Utc>,
    message: Uuid,
    /// The session's own words, when it said them (not only the boss's
    /// hold).
    said: Option<LineMessage>,
    /// Its boss's newest word to it holds it for an answer: the boss has
    /// said it won't answer now, so the person is told at once.
    held: bool,
}

/// Whether `m` is the boss's word to `s`, as the session got it.
fn bosss_word(m: &LineMessage, boss: &str, s: Uuid) -> bool {
    m.from.kind == PartyKind::Manager
        && m.from.id == boss
        && m.to_session() == Some(s)
        && m.delivery == Delivery::Delivered
}

/// Whether `m` is session `s`'s word to `boss`.
fn to_boss(m: &LineMessage, boss: &str, s: Uuid) -> bool {
    m.from_session() == Some(s) && m.to.kind == PartyKind::Manager && m.to.id == boss
}

/// When `m` reached its recipient, or was written.
fn reached(m: &LineMessage) -> DateTime<Utc> {
    m.delivered_at.unwrap_or(m.at)
}

/// Of `thread` (oldest first), what of session `s` waits on `boss`.
fn waiting_in(thread: &[LineMessage], boss: &str, s: Uuid) -> Option<Waiting> {
    let ours = thread.iter().rfind(|m| bosss_word(m, boss, s));
    let theirs = thread.iter().rfind(|m| to_boss(m, boss, s));
    let held = ours.is_some_and(|o| workforce_desk::line::holds(&o.text));
    match (theirs, ours) {
        // Its word is the newest: the ball is in the boss's court when it
        // waits on an answer.
        // A question asked on the line has its own way to the person
        // (escalated, or lapsed as it goes ahead): only a message waits
        // with nothing to take it there.
        (Some(t), o) if o.is_none_or(|o| reached(o) < t.at) => {
            // Spoken since the boss's hold, its own words say whether it
            // still waits ("Done, merged" doesn't).
            let waits = t.kind == LineKind::Tell && waits_on_answer(&t.text);
            // Under the boss's hold, the wait began with the hold.
            let began = ours.filter(|_| held);
            waits.then(|| Waiting {
                since: began.map_or(t.at, reached),
                message: began.map_or(t.id, |o| o.id),
                said: Some(t.clone()),
                held,
            })
        }
        // The boss's hold is the newest, and the session stopped on it.
        (_, Some(o)) if held => Some(Waiting {
            since: reached(o),
            message: o.id,
            said: theirs.filter(|t| t.at < reached(o)).cloned(),
            held,
        }),
        _ => None,
    }
}

impl CrewHub {
    /// When the person last took part in session `s` after `since`, and
    /// in what words when the picture kept them: what they typed while the
    /// hub ran, or what the picture kept across a restart.
    fn person_since(
        &self,
        boss: &str,
        s: Uuid,
        since: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, Option<String>)> {
        let kept = self
            .desk
            .picture_recent(boss, s, LOOK_BACK as usize)
            .into_iter()
            .filter(|r| {
                r.what == workforce_desk::picture::What::PersonSaid
                    && r.session == Some(s)
                    && r.at > since
            })
            .max_by_key(|r| r.at);
        let typed = self.desk.person_input_at(s).filter(|t| *t > since);
        match (typed, kept) {
            (Some(t), Some(r)) if r.at >= t - chrono::Duration::seconds(5) => {
                Some((t, Some(r.text)))
            }
            (Some(t), _) => Some((t, None)),
            (None, Some(r)) => Some((r.at, Some(r.text))),
            (None, None) => None,
        }
    }

    /// What the person calls boss `boss`.
    fn boss_name(&self, boss: &str) -> String {
        self.overseer_named(boss)
            .map(|(s, _)| super::name_of(&s))
            .unwrap_or_else(|| "Desk".into())
    }

    /// The bound before what waits on a boss is the person's.
    fn waiting_bound(&self, o: &Oversight) -> chrono::Duration {
        chrono::Duration::from_std(
            self.timing
                .minute
                .mul_f64(f64::from(o.wake.waiting_after_minutes)),
        )
        .unwrap_or_default()
    }

    /// Whether `view` stands stopped on its boss: live, its turn over,
    /// nothing else it waits on (a request, its own work, a question the
    /// person already has), and not the person's to drive now.
    fn stopped_on_boss(&self, view: &SessionView, boss: &str) -> bool {
        view.live
            && matches!(view.state, SessionState::Done | SessionState::Idle)
            && view.pending.is_none()
            && view.background.is_none()
            && !view.reading
            && !self.person_has_questions(view, boss)
    }

    /// Looks at every session a boss runs or oversees: what has waited on
    /// its boss past the bound is given to the person.
    pub(crate) async fn look_for_silent_waits(&self) {
        let now = Utc::now();
        for view in self.desk.list() {
            let Some(boss) = self.boss_over(&view) else {
                continue;
            };
            let Some((_, o)) = self.overseer_named(&boss) else {
                continue;
            };
            if o.wake.waiting_after_minutes == 0 || !self.stopped_on_boss(&view, &boss) {
                continue;
            }
            let thread = match self.desk.line_thread(view.id, None, Some(LOOK_BACK)).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::info!(session = %view.id, error = %e, "a session's line couldn't be read for what waits");
                    continue;
                }
            };
            let Some(w) = waiting_in(&thread, &boss, view.id) else {
                continue;
            };
            if !w.held && now - w.since < self.waiting_bound(&o) {
                continue;
            }
            // The person took part in it since: theirs, not a silent wait.
            if self.person_since(&boss, view.id, w.since).is_some() {
                continue;
            }
            self.give_wait_to_person(&boss, &o, &view, &w, now).await;
        }
    }

    async fn give_wait_to_person(
        &self,
        boss: &str,
        o: &Oversight,
        view: &SessionView,
        w: &Waiting,
        now: DateTime<Utc>,
    ) {
        let store = self.desk.store();
        // Answered from the item, its answer goes to the session in reply
        // to its message; a question of its own has its own answer.
        let line = w
            .said
            .as_ref()
            .filter(|m| m.kind == LineKind::Tell)
            .map(|m| m.id);
        // Each wait is given to the person once: settled, it stays so.
        let key = key(view.id, w.message);
        match store.item_ever_keyed(boss, &key).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::info!(error = %e, "whether a wait was given to the person couldn't be read");
                return;
            }
        }
        // Already the person's: its question raised, or the boss asked
        // them since it began to wait (about it, or with no session named).
        let open = match store.items_not_closed(Some(boss)).await {
            Ok(open) => open,
            Err(e) => {
                tracing::info!(error = %e, "a boss's open items couldn't be read for what waits");
                return;
            }
        };
        let theirs = open.iter().any(|i| {
            !is_waiting(&i.dedupe_key)
                && (line.is_some() && i.line_message == line
                    || matches!(i.kind, OpenKind::Ask | OpenKind::Confirm)
                        && i.said()
                        && i.opened_at >= w.since
                        && i.session.is_none_or(|s| s == view.id))
        });
        if theirs {
            return;
        }
        let name = view.label().to_string();
        let words = w
            .said
            .as_ref()
            .map(|m| workforce_desk::line::without_marks(&m.text))
            .unwrap_or_default();
        let question = crew::question_in(&words)
            .or_else(|| {
                words
                    .split(['\n', '.'])
                    .map(str::trim)
                    .find(|s| WAITS.iter().any(|x| s.to_lowercase().contains(x)))
                    .map(str::to_string)
            })
            .map(|q| crew::one_line(&q, 300));
        let headline = match &question {
            Some(q) => format!("{name} waits on a decision: {q}"),
            None => format!(
                "{name} is holding, waiting on {}'s answer",
                self.boss_name(boss)
            ),
        };
        let mut text = headline.clone();
        if !words.trim().is_empty() {
            text.push_str("\n\n");
            text.push_str(&crew::clip(words.trim(), KEPT_CHARS));
        }
        let mut new = NewItem::ask(boss, OpenKind::Ask, Some(view.id), &text);
        new.dedupe_key = key.clone();
        new.line_message = line;
        new.options = options_in(&words);
        new.recommendation = recommendation_in(&words);
        new.layout.question = Some(headline);
        new.layout.recommended = new.recommendation.clone();
        new.holding_up = Some(format!(
            "{name} has waited {} and changes nothing until it is answered",
            waited(w.since, now)
        ));
        new.cant_decide = Some(format!(
            "No answer reached {name} within {} min, so it's yours to decide",
            o.wake.waiting_after_minutes
        ));
        // What it waited on before is this wait now: one item a session.
        let earlier = format!("{KEY}{}:", view.id);
        if let Err(e) = store
            .item_close_of_key_prefix(&earlier, ClosedBy::Session, "It waits on something newer.")
            .await
        {
            tracing::info!(error = %e, "a session's earlier wait wasn't closed");
        }
        let opened = match store.item_open(&new).await {
            Ok(Opened::New(i)) => i,
            Ok(Opened::Again(_)) => return,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "keeping a session's wait for the person failed");
                return;
            }
        };
        tracing::info!(session = %view.id, item = %opened.id, "a session waited on its boss past the bound: given to the person");
        self.post_item(boss, &opened, super::items::needs_you(&opened), Vec::new())
            .await;
    }

    /// The person's `answer` to waiting item `i`: to its session, in reply
    /// to what it waits on when that was its message, and the item closed.
    pub(super) async fn answer_wait(&self, i: &OpenItem, answer: &str) -> Result<(), String> {
        let refused = |e: workforce_desk::LineError| match e {
            workforce_desk::LineError::Refused(r) => r.to_string(),
            workforce_desk::LineError::Desk(e) => format!("Desk couldn't send it: {e}"),
        };
        match (
            i.line_message,
            i.session.or_else(|| session_of(&i.dedupe_key)),
        ) {
            (Some(m), _) => {
                self.desk
                    .line_answer(m, workforce_desk::LineSender::Person, answer.to_string())
                    .await
                    .map_err(refused)?;
            }
            (None, Some(s)) => {
                self.desk
                    .line_send(
                        workforce_desk::LineSender::Person,
                        workforce_desk::line::Send::Tell {
                            to: workforce_desk::line::Target::Session(s),
                            text: answer.to_string(),
                            when: workforce_desk::line::When::Queue,
                        },
                    )
                    .await
                    .map_err(refused)?;
            }
            (None, None) => return Err("Its session is gone.".into()),
        }
        self.desk
            .store()
            .item_close(i.id, ClosedBy::Person, &format!("You answered: {answer}"))
            .await
            .map_err(|e| format!("The answer went, but the item didn't close: {e}"))?;
        Ok(())
    }

    /// What closes waiting item `i`, when something did: the person's own
    /// input into the session, the boss's word reaching it, or the session
    /// going on; with the one line that says so.
    pub(super) async fn waiting_past(&self, i: &OpenItem) -> Option<(ClosedBy, String)> {
        let s = i.session.or_else(|| session_of(&i.dedupe_key))?;
        let view = match self.desk.view(s) {
            Ok(v) => v,
            Err(workforce_desk::DeskError::NotFound) => {
                return Some((ClosedBy::Session, "Its session is gone.".into()))
            }
            Err(_) => return None,
        };
        let name = view.label().to_string();
        let tz = self.desk.timezone();
        let at = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
        if view.state == SessionState::Closed {
            return Some((
                ClosedBy::Session,
                format!("{name} ended; I've closed the question."),
            ));
        }
        if let Some((t, kept)) = self.person_since(&i.boss, s, i.opened_at) {
            let words = kept
                .or_else(|| view.last_prompt.clone())
                .as_deref()
                .map(|p| crew::words(&crew::one_line(p, 200), 12))
                .filter(|p| !p.trim().is_empty())
                .map(|p| format!(" (“{p}”)"))
                .unwrap_or_default();
            return Some((
                ClosedBy::Person,
                format!(
                    "You settled it in {name} at {}{words}; I've closed the question.",
                    at(t)
                ),
            ));
        }
        let thread = self.desk.line_thread(s, None, Some(LOOK_BACK)).await.ok()?;
        if let Some(m) = thread.iter().rfind(|m| {
            bosss_word(m, &i.boss, s)
                && reached(m) > i.opened_at
                && !workforce_desk::line::holds(&m.text)
        }) {
            return Some((
                ClosedBy::Boss,
                format!(
                    "{} answered {name} at {} (“{}”); I've closed the question.",
                    self.boss_name(&i.boss),
                    at(reached(m)),
                    crew::words(&crew::one_line(&m.text, 200), 12)
                ),
            ));
        }
        if matches!(view.state, SessionState::Working | SessionState::Blocked)
            || thread
                .iter()
                .any(|m| to_boss(m, &i.boss, s) && m.at > i.opened_at && !waits_on_answer(&m.text))
        {
            return Some((
                ClosedBy::Session,
                format!("{name} went on without waiting; I've closed the question."),
            ));
        }
        None
    }

    /// Says again each waiting item past its time, on the boss's cadence:
    /// how long its session has waited; never in quiet hours or while the
    /// person is away, and never parked.
    pub(super) async fn remind_waiting(&self, items: &[OpenItem]) {
        let now = Utc::now();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let tz = self.desk.timezone();
        for i in items.iter().filter(|i| {
            is_waiting(&i.dedupe_key)
                && i.state == workforce_desk::open_item::OpenState::Open
                && i.said()
        }) {
            let Some((_, o)) = self.overseer_named(&i.boss) else {
                continue;
            };
            let Some(gap) = remind_after(&o.wake.waiting_remind_minutes, i.restated()) else {
                continue;
            };
            let last = i.last_said_at.unwrap_or(i.opened_at);
            if now - last < minute * gap as i32 {
                continue;
            }
            // Never in quiet hours nor held for a roundup (a held card
            // would come with the ones after it), and never while away:
            // their catch-up lists what waits.
            if workforce_desk::update_cadence::held_until(&o.updates.cadence, now, &tz).is_some()
                || self.away_now(&i.boss).await
            {
                continue;
            }
            // Since it began to wait: the bound before it was given.
            let since = i.opened_at - self.waiting_bound(&o);
            let text = format!(
                "**Still waiting on you** ({} now). {}",
                waited(since, now),
                i.headline()
            );
            if self.post_item(&i.boss, i, text, Vec::new()).await {
                if let Err(e) = self.desk.store().item_said(i.id, now, false).await {
                    tracing::warn!(error = %e, "counting a waiting item's reminder failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_laid_out_or_a_hold_waits_on_the_boss() {
        let report = "The REINDEX can't fix the drill. Both need your decision.\n\n\
                      (a) Upgrade staging to 16.16.\n(b) Pin the drill's minor.\n(c) Accept it.\n\
                      I recommend (a) on staging in the day.";
        assert!(waits_on_answer(report));
        assert_eq!(options_in(report).len(), 3, "{:?}", options_in(report));
        assert_eq!(options_in(report)[1], "Pin the drill's minor.");
        assert_eq!(
            recommendation_in(report).as_deref(),
            Some("I recommend (a) on staging in the day")
        );
        assert!(waits_on_answer(
            "Holding: I'm changing nothing until Desk answers."
        ));
        assert!(!waits_on_answer(
            "Done: the date picker is on 4.2 and the tests pass."
        ));
        assert!(options_in("Fixed 3 tests.\n2 left.").is_empty());
        // What was done, listed, is no choice.
        let done = "Done.\n1. Updated the migration.\n2. Ran the tests.\nCI: green on all jobs.\nPR: opened #12.";
        assert!(options_in(done).is_empty(), "{:?}", options_in(done));
        assert!(!waits_on_answer(done));
        assert!(!waits_on_answer(
            "Wired up your callback and the until-desktop flag."
        ));
        // Options alone, without a recommendation or a question, are a list.
        assert!(!waits_on_answer(
            "Tried:\n(a) the 16.15 image\n(b) the 16.16 image"
        ));
        assert_eq!(
            options_in("Option A: upgrade staging first\nOption B: pin the minor").len(),
            2
        );
        // Text that changes length in lower case never panics.
        let _ = options_in("Option Éé: İ foo bar\nİİİİ Option: x y");
        let _ = waits_on_answer("İİİİ until Desk answers; K your call");
    }

    #[test]
    fn reminders_come_on_the_cadence_and_the_last_repeats() {
        let c = [10, 30, 60];
        assert_eq!(remind_after(&c, 0), Some(10));
        assert_eq!(remind_after(&c, 1), Some(30));
        assert_eq!(remind_after(&c, 2), Some(60));
        assert_eq!(remind_after(&c, 9), Some(60));
        assert_eq!(remind_after(&[0, 0, 0], 0), None, "off");
    }
}
