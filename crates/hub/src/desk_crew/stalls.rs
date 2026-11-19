//! No update says work is stuck with nobody on it (19 Nov, 14:51: "The
//! Workforce track 2 PRs haven't started, because no answers to the plan's
//! open questions are recorded", when the person had answered all eight at
//! 11:57 and the boss held their answers; and "Legacy removal is blocked…",
//! with nothing for anyone to do). The person reads hundreds of things: a
//! stall said as prose alone sits for hours.
//!
//! Every item an update says about blocked or stalled work
//! ([`write::stalled`]) says who moves it next and what happens then, and
//! that is done, before it is written:
//!
//! 1. **What the boss holds.** The session waits on the person's word, and
//!    the boss holds it (its orders: [`super::held`]): it is given to the
//!    session now, and the item says "I have your answers…", never that
//!    they are missing.
//! 2. **The person's already.** An open question of theirs about it: the
//!    item says it ("Yours: …"), its card answers it.
//! 3. **The boss acted.** It did something about it since the session said
//!    so: what it did.
//! 4. **Its session is on it.** Working, waiting on nothing: what it does.
//! 5. **It waits on the boss.** Its full turn is handed it now.
//! 6. **Only the person.** It waits on their word: the waiting item a silent
//!    wait is ([`super::waiting`]), its card answering straight to the
//!    session and reminded until it is, closing in one line when settled.
//! 7. **Outside the crew.** It waits on something else (CI, a review): "I'll
//!    check again at 15:20", kept as the boss's promise and looked at then
//!    ([`CrewHub::look_at_rechecks`]).
//! 8. **Otherwise** the boss's full turn is handed it: "I'm looking at what
//!    unblocks it now."
//!
//! Where the boss doesn't act (Watch, Suggest), its update is handed to a
//! check-in, which is that full turn: nothing more is woken.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use workforce_common::oversight::Oversight;
use workforce_desk::decision::DecisionKind;
use workforce_desk::model::SessionState;
use workforce_desk::open_item::{ClosedBy, NewItem, OpenItem, OpenKind, OpenState};
use workforce_desk::open_item_store::Opened;
use workforce_desk::update_write::{self as write, Mover, Said, Work};

use super::updates::{Batch, Changed};
use super::CrewHub;

/// What a re-check's key starts with: `recheck:{session}:{when}`.
pub(crate) const RECHECK: &str = "recheck:";

/// How long after a stall is handed to the boss's full turn it may be
/// handed again, in minutes.
const HANDED_AGAIN: i32 = 60;

/// The fewest minutes before a re-check.
const RECHECK_MINUTES: u32 = 15;

/// The key of a re-check of session `s` at `due`.
fn recheck_key(s: Uuid, due: DateTime<Utc>) -> String {
    format!("{RECHECK}{s}:{}", due.to_rfc3339())
}

/// The session a re-check is of, and when it is due.
pub(crate) fn recheck_of(key: &str) -> Option<(Uuid, DateTime<Utc>)> {
    let (s, due) = key.strip_prefix(RECHECK)?.split_once(':')?;
    Some((
        s.parse().ok()?,
        DateTime::parse_from_rfc3339(due).ok()?.with_timezone(&Utc),
    ))
}

/// Whether `waiting_on` names the boss (Desk, or its name): its own to
/// answer.
fn names_boss(waiting_on: &str, boss_name: &str) -> bool {
    let l = waiting_on.to_lowercase();
    let word = |w: &str| {
        let w = w.to_lowercase();
        !w.is_empty()
            && l.split(|c: char| !c.is_alphanumeric())
                .any(|t| t == w.as_str())
    };
    word("desk") || word(boss_name) || word("boss") || word("overseer")
}

/// Whether `waiting_on` names the person: "you", "the owner", "Kingsley".
fn names_person(waiting_on: &str, person: Option<&str>) -> bool {
    let l = waiting_on.to_lowercase();
    let words: Vec<&str> = l.split(|c: char| !c.is_alphanumeric()).collect();
    ["you", "your", "owner", "person", "user"]
        .iter()
        .any(|w| words.contains(w))
        || person
            .map(|p| p.to_lowercase())
            .is_some_and(|p| !p.is_empty() && words.contains(&p.as_str()))
}

/// The newest of what a session said that waits on someone, from its
/// changed account: its newest line that says so, else what it waits on.
fn words_of(c: Option<&Changed>) -> String {
    let Some(c) = c else {
        return String::new();
    };
    c.new
        .iter()
        .rev()
        .map(|d| d.text.trim())
        .find(|t| write::says_stall(t) || write::says_missing(t) || write::asks_for_word(t))
        .map(str::to_string)
        .or_else(|| c.waiting_on.clone())
        .unwrap_or_default()
}

/// The exact question item `i` puts to the person, in full: never its
/// clipped headline.
fn asked(i: &OpenItem) -> String {
    i.layout
        .question
        .clone()
        .filter(|q| !q.trim().is_empty())
        .unwrap_or_else(|| workforce_desk::crew::one_line(&i.text, 300))
}

impl CrewHub {
    /// Who moves each item of `said` that is about blocked or stalled work
    /// ([`write::stalled`]), set on it and done: the session given what the
    /// boss holds, the person given the question as an item, a re-check
    /// kept, or the boss's full turn handed it.
    pub(super) async fn own_stalls(
        &self,
        boss: &str,
        o: &Oversight,
        b: &Batch,
        said: &mut [Said],
        now: DateTime<Utc>,
    ) {
        let tz = self.desk.timezone();
        let acts = self.level_now(boss, o).0.acts();
        for s in said.iter_mut() {
            let Some(sid) = s
                .session
                .as_ref()
                .and_then(|l| l.session_id.parse::<Uuid>().ok())
            else {
                continue;
            };
            let changed = b.sessions.iter().find(|c| c.id == sid);
            let lines: Vec<&str> = changed
                .map(|c| {
                    c.new
                        .iter()
                        .map(|d| d.text.as_str())
                        .chain(c.waiting_on.as_deref())
                        .collect()
                })
                .unwrap_or_default();
            if !write::stalled(s, &lines) {
                continue;
            }
            let Ok(view) = self.desk.view(sid) else {
                continue;
            };
            let work = match changed {
                Some(c) => c.work.clone(),
                None => self.work_of(&view, None, &[], None).await,
            };
            let all = lines
                .iter()
                .copied()
                .chain([s.headline.as_str(), s.body.as_str()])
                .collect::<Vec<_>>()
                .join("\n");
            // When the session last said where it is: what reached it since
            // answers it.
            let since = changed
                .and_then(|c| c.new.iter().map(|d| d.at).max())
                .or(s.at)
                .unwrap_or(now);
            // 1. The person's word, which the boss holds: given, and said so.
            if let Some(held) = b.held.get(&sid).filter(|h| !h.is_empty()) {
                if write::asks_for_word(&all) || write::says_missing(&all) {
                    let doing = self.give_held(boss, sid, held, since, acts).await;
                    *s = write::held(s.clone(), &work, &doing, &tz);
                    continue;
                }
            }
            let m = self
                .mover_of(boss, o, b, &view, changed, &work, &all, since, acts, now)
                .await;
            *s = write::moved(s.clone(), m, &work, &tz);
        }
    }

    /// Gives session `s` the person's word `boss` holds for it (`held`),
    /// and what the item says the boss is doing about it.
    async fn give_held(
        &self,
        boss: &str,
        s: Uuid,
        held: &[OpenItem],
        since: DateTime<Utc>,
        acts: bool,
    ) -> String {
        let mut went = false;
        let mut failed: Option<String> = None;
        for i in held {
            match self.deliver_held(boss, s, i, since).await {
                Ok(_) => went = true,
                Err(why) => failed = Some(why),
            }
        }
        match (went, failed, acts) {
            (true, _, true) => "I'm passing them to its session now.".into(),
            (true, _, false) => {
                "I've put passing them to its session to you beside this message.".into()
            }
            (false, Some(why), true) => {
                self.hand_stall(
                    boss,
                    s,
                    format!(
                        "{} waits on the person's word, which you hold as their order, and Desk \
                         couldn't give it to the session ({why}). Give it to the session now \
                         (crew.nudge), and note it on its ticket.",
                        self.session_name(s)
                    ),
                );
                "I'm getting them to its session now.".into()
            }
            (false, _, _) => "I'm looking at how to get them to its session now.".into(),
        }
    }

    /// Who moves session `view`'s stall (`all`, what it and the item say of
    /// it, since `since`), past what the boss holds: done here.
    #[allow(clippy::too_many_arguments)]
    async fn mover_of(
        &self,
        boss: &str,
        o: &Oversight,
        b: &Batch,
        view: &workforce_desk::model::SessionView,
        changed: Option<&Changed>,
        work: &Work,
        all: &str,
        since: DateTime<Utc>,
        acts: bool,
        now: DateTime<Utc>,
    ) -> Mover {
        let sid = view.id;
        let waiting_on = changed.and_then(|c| c.waiting_on.as_deref());
        // 2. The person's already: their open question about it.
        if let Some(i) = b.open.iter().find(|i| {
            matches!(i.kind, OpenKind::Ask | OpenKind::Confirm | OpenKind::Todo)
                && i.state != OpenState::Closed
                && i.said()
                && i.linked().contains(&sid)
        }) {
            return Mover::Person {
                question: asked(i),
                item: i.id,
            };
        }
        // 3. The boss did something about it since the session said so.
        if let Some(d) = b.decisions.iter().rev().find(|d| {
            d.session == Some(sid)
                && d.kind == DecisionKind::Acted
                && d.at >= since
                && !d.brief.trim().is_empty()
        }) {
            return Mover::Boss {
                doing: format!("I {}", write::lowered(d.brief.trim())),
            };
        }
        let boss_name = self.boss_name_of(boss);
        let on_boss = waiting_on.is_some_and(|w| names_boss(w, &boss_name))
            || self.waits_on_boss_line(boss, sid).await;
        // 4. Its session is on it, waiting on nothing.
        if view.live && view.state == SessionState::Working && waiting_on.is_none() && !on_boss {
            let doing = changed.map(|c| c.doing.trim()).unwrap_or_default();
            return Mover::Boss {
                doing: match doing {
                    "" => "I have its session working on it now.".into(),
                    d => format!("I have its session on it now: {}", write::lowered(d)),
                },
            };
        }
        // 5. It waits on the boss: its full turn answers it.
        if on_boss {
            if acts {
                self.hand_stall(
                    boss,
                    sid,
                    format!(
                        "{} waits on your answer{}. Answer it now (crew.nudge); give it to the \
                         person with crew.ask_person only if only they can settle it, after \
                         checking your orders, your DM with them, its line, its ticket and its \
                         plan for what they already said.",
                        view.label(),
                        waiting_on
                            .map(|w| format!(" ({})", workforce_desk::crew::one_line(w, 200)))
                            .unwrap_or_default()
                    ),
                );
            }
            return Mover::Boss {
                doing: "I'm answering it now.".into(),
            };
        }
        // 6. Only the person: their word, as an item with its answer.
        let person = self.person(None);
        let theirs = waiting_on.is_some_and(|w| names_person(w, person.as_deref()))
            || write::asks_for_word(all)
            || write::says_missing(all);
        if theirs {
            let words = words_of(changed);
            let question = match waiting_on {
                Some(w) => format!("{} waits on {}", work.in_words(), write::lowered(w)),
                None => format!(
                    "{} waits on your word: {}",
                    work.in_words(),
                    workforce_desk::crew::one_line(&words, 200)
                ),
            };
            if let Some(i) = self
                .give_stall_to_person(boss, view, &question, &words, now)
                .await
            {
                return Mover::Person {
                    question: asked(&i),
                    item: i.id,
                };
            }
        }
        // 7. Outside the crew: looked at again, and when.
        if let Some(w) = waiting_on.filter(|_| !theirs) {
            if let Some(due) = self.keep_recheck(boss, o, view, w, now).await {
                return Mover::Outside {
                    on: w.to_string(),
                    check_at: due,
                };
            }
        }
        // 8. Nobody: the boss's full turn is handed it.
        if acts {
            self.hand_stall(
                boss,
                sid,
                format!(
                    "Your update tells the person {} is blocked, and nobody moves it: {}. Move it \
                     now: unblock it, hand it on, or, only if just the person can, ask them with \
                     crew.ask_person. Before saying anything is missing, check your orders, your \
                     DM with them, its line, its ticket and its plan.",
                    view.label(),
                    workforce_desk::crew::one_line(all, 300)
                ),
            );
        }
        Mover::Boss {
            doing: "I'm looking at what unblocks it now.".into(),
        }
    }

    /// What the person calls boss `boss`.
    fn boss_name_of(&self, boss: &str) -> String {
        self.overseer_named(boss)
            .map(|(s, _)| super::name_of(&s))
            .unwrap_or_else(|| "Desk".into())
    }

    /// Whether session `s`'s line has the ball in `boss`'s court: its newest
    /// word waits on the boss's answer ([`super::waiting::waiting_in`]).
    async fn waits_on_boss_line(&self, boss: &str, s: Uuid) -> bool {
        match self.desk.line_thread(s, None, Some(30)).await {
            Ok(thread) => super::waiting::waiting_in(&thread, boss, s).is_some(),
            Err(_) => false,
        }
    }

    /// Hands `boss`'s full turn `line` about session `s`'s stall, unless
    /// it was handed that session's within the hour.
    fn hand_stall(&self, boss: &str, s: Uuid, line: String) {
        let now = Utc::now();
        let minute = Duration::from_std(self.timing.minute).unwrap_or_default();
        {
            let mut handed = self.stalls_handed.lock();
            let key = (boss.to_string(), s);
            if handed
                .get(&key)
                .is_some_and(|at| now - *at < minute * HANDED_AGAIN)
            {
                return;
            }
            handed.insert(key, now);
        }
        self.wake_on(boss, s, line, format!("stall:{s}"));
    }

    /// Wakes `boss`'s full turn with `line` about session `s`.
    fn wake_on(&self, boss: &str, s: Uuid, line: String, about: String) {
        let Some((service, o)) = self.overseer_named(boss) else {
            tracing::info!(%boss, "a stall has no boss here to hand it to");
            return;
        };
        let group = self.desk.view(s).ok().and_then(|v| v.group.map(|g| g.id));
        let t = super::watcher::Trigger {
            line,
            approvals: Vec::new(),
            hop: 0,
            about: Some(about),
            group,
            handed: None,
        };
        let crew = self.me();
        tokio::spawn(async move {
            crew.wake(service, o, vec![t]).await;
        });
    }

    /// Keeps `boss`'s promise to look again at session `view`, which waits
    /// on `on`, outside the crew: when that is (the one kept already, while
    /// it is still to come).
    async fn keep_recheck(
        &self,
        boss: &str,
        o: &Oversight,
        view: &workforce_desk::model::SessionView,
        on: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let store = self.desk.store();
        let open = store.items_not_closed(Some(boss)).await.ok()?;
        let prefix = format!("{RECHECK}{}:", view.id);
        if let Some(due) = open
            .iter()
            .filter(|i| i.dedupe_key.starts_with(&prefix))
            .filter_map(|i| recheck_of(&i.dedupe_key).map(|(_, d)| d))
            .filter(|d| *d > now)
            .min()
        {
            return Some(due);
        }
        let minute = Duration::from_std(self.timing.minute).unwrap_or_default();
        let after = o.updates.every_minutes.max(RECHECK_MINUTES);
        let due = now + minute * after as i32;
        let tz = self.desk.timezone();
        let on = workforce_desk::crew::one_line(on, 200);
        let mut new = NewItem::ask(
            boss,
            OpenKind::Promise,
            Some(view.id),
            &format!(
                "Check again at {} on {}: it waits on {on}.",
                due.with_timezone(&tz).format("%H:%M"),
                view.label()
            ),
        );
        new.dedupe_key = recheck_key(view.id, due);
        new.said = false;
        new.sessions = vec![view.id];
        new.holding_up = Some(on);
        match store.item_open(&new).await {
            Ok(Opened::New(_) | Opened::Again(_)) => Some(due),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "keeping a re-check failed");
                None
            }
        }
    }

    /// Looks at each re-check due (`items`, every open item): closed, and
    /// where its session still waits on the same, its boss's full turn
    /// handed it.
    pub(super) async fn look_at_rechecks(&self, items: &[OpenItem]) {
        let now = Utc::now();
        let store = self.desk.store();
        for i in items {
            let Some((s, due)) = recheck_of(&i.dedupe_key) else {
                continue;
            };
            if i.state == OpenState::Closed || due > now {
                continue;
            }
            let tz = self.desk.timezone();
            let at = due.with_timezone(&tz).format("%H:%M").to_string();
            if let Err(e) = store
                .item_close(i.id, ClosedBy::Boss, &format!("Checked again at {at}."))
                .await
            {
                tracing::warn!(error = %e, "closing a re-check failed");
                continue;
            }
            let on = i.holding_up.clone().unwrap_or_default();
            let still = match self.desk.session_accounts_of(&[s]).await {
                Ok(a) => a.iter().any(|a| {
                    a.waiting_on.as_deref().map(str::trim).is_some_and(|w| {
                        !w.is_empty() && workforce_desk::crew::one_line(w, 200) == on
                    })
                }),
                Err(_) => true,
            };
            let live = self
                .desk
                .view(s)
                .is_ok_and(|v| v.state != SessionState::Closed);
            if !still || !live {
                continue;
            }
            self.wake_on(
                &i.boss,
                s,
                format!(
                    "You told the person you'd check again at {at} on {}, which waits on {on}. \
                     Check it now: if it moved, carry on; if not, do what moves it, or say when \
                     you'll look next.",
                    self.session_name(s)
                ),
                format!("recheck:{s}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rechecks_key_says_its_session_and_when() {
        let s = Uuid::new_v4();
        let due = DateTime::parse_from_rfc3339("2026-11-19T15:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(recheck_of(&recheck_key(s, due)), Some((s, due)));
        assert_eq!(recheck_of("waiting:x:y"), None);
    }

    #[test]
    fn who_is_waited_on_is_read_from_its_words() {
        assert!(names_boss("Desk's go-ahead", "Desk"));
        assert!(!names_boss("the desktop build", "Desk"));
        assert!(names_person("the owner's answers", None));
        assert!(names_person("Kingsley to approve", Some("Kingsley")));
        assert!(!names_person("CI on the wasmtime PR", Some("Kingsley")));
    }
}
