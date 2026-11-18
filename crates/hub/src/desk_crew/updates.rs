//! A boss's updates (overseer H7): what changed on its crew since it last
//! told the person, gathered and sent as one message in its DM.
//!
//! - **What is news.** A session's account that did something (a done
//!   line) or changed what comes next or what it waits on; a decision the
//!   boss made; a promise or order it kept; an ask due to be said again.
//!   What a session is doing this minute alone is not.
//! - **When.** A visible result (a PR, merged, deployed, failed, a
//!   decision, a kept order) starts the gather and goes out
//!   `every_minutes` after; a small change by `max_wait_minutes`. Never two
//!   within `every_minutes`, doubling after three in a row the person
//!   hasn't read. Its state is in Postgres (`boss_update_state`), and the
//!   update is kept before it is sent, so a restart neither loses nor
//!   repeats it.
//! - **Who writes it.** The cheap model writes each session's paragraph
//!   in the boss's voice ([`UpdateWriter`]), and the short line and why of
//!   the newest decisions it acted on (never quoting the message it sent;
//!   one that doesn't fit is the row's). The text's decision lines, what
//!   it kept and what still waits are the rows' own words. Off, failed or
//!   past the day's ceiling, all of it is written plainly from the rows.
//! - **Where.** At Act and Act fully the update is its own message
//!   (`DeliveryKind::Update`). At Watch and Suggest it is handed to a
//!   check-in, whose message (with its suggestions) is the one post; with
//!   the check-ins spent, it goes out plainly as its own.
//!
//! Needs-you and decisions never wait on any of this: they are posted and
//! recorded as they happen.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Oversight;
use workforce_common::oversight::{Cadence, RoundupEvery, Route};
use workforce_desk::account::{Account, Done};
use workforce_desk::crew::Extras;
use workforce_desk::decision::{BossDecision, DecisionKind, DecisionLine, Need};
use workforce_desk::model::SessionState;
use workforce_desk::open_item::{OpenItem, OpenKind, OpenState};
use workforce_desk::update::{
    self, Change, MentionedUpdate, NeedsYou, Pending, Told, UpdateSession, UpdateState, WaitingItem,
};
use workforce_desk::update_cadence::{self as cadence, Roundup, SectionKey};
use workforce_desk::update_routine as routine;
use workforce_desk::update_write::{self as write, EventKind, Said, Titled, Work};

use super::watcher::Budget;
use super::{CrewHub, CHECK_IN};
use crate::serve::AgentService;

/// Who writes an update's session paragraphs: the cheap model, on the
/// boss's own plan for the person.
#[async_trait::async_trait]
pub(crate) trait UpdateWriter: Send + Sync {
    async fn write(
        &self,
        boss: &str,
        owner: &str,
        system: &str,
        user: &str,
    ) -> eyre::Result<String>;

    /// What `boss`'s next update call is paid by.
    async fn billing(&self, _boss: &str) -> workforce_desk::account::Billing {
        workforce_desk::account::Billing::Metered
    }
}

/// The hub's writer: the floor's cheap model, booked as `desk:update`.
pub(crate) struct HubWriter {
    pub model: crate::floor::gists::HubModel,
}

#[async_trait::async_trait]
impl UpdateWriter for HubWriter {
    async fn write(
        &self,
        boss: &str,
        owner: &str,
        system: &str,
        user: &str,
    ) -> eyre::Result<String> {
        let subject = workforce_desk::floor::Subject::Agent {
            agent_id: boss.to_string(),
            session_id: String::new(),
        };
        self.model
            .answer_for(
                Some(&subject),
                Some(owner),
                system,
                user,
                crate::floor::gists::Call::UPDATE,
            )
            .await
            .map(|(text, _)| text)
    }

    async fn billing(&self, boss: &str) -> workforce_desk::account::Billing {
        let subject = workforce_desk::floor::Subject::Agent {
            agent_id: boss.to_string(),
            session_id: String::new(),
        };
        self.model
            .billing(Some(&subject), crate::floor::gists::Call::UPDATE)
            .await
    }
}

/// A session that changed since the person was last told of it.
pub(super) struct Changed {
    pub id: Uuid,
    pub name: String,
    /// The repository it works in, by name.
    repo: Option<String>,
    /// Its account as the update tells of it ([`update::as_told`]).
    account: Account,
    new: Vec<Done>,
    /// What its work is, from facts: its goal and its tickets' and PRs'
    /// titles.
    pub work: Work,
}

impl Changed {
    /// What the update names it by: its name and repository.
    fn context(&self) -> String {
        update::subject(&self.name, self.repo.as_deref())
    }
}

/// The repository session `v` works in, by name: a PR's among `new`, else
/// its git project's folder.
fn repo_of(v: &workforce_desk::model::SessionView, new: &[Done]) -> Option<String> {
    let refs: Vec<&workforce_desk::account::Ref> = new.iter().flat_map(|d| &d.refs).collect();
    // A folder in no repository names none.
    let project = workforce_desk::git_project(&v.cwd);
    let in_git = std::path::Path::new(&project).join(".git").exists();
    update::repo_of(&refs, if in_git { &project } else { "" })
}

/// What one boss's next update would say now.
pub(super) struct Batch {
    pub change: Change,
    pub sessions: Vec<Changed>,
    pub decisions: Vec<BossDecision>,
    pub kept: Vec<String>,
    waiting: Vec<OpenItem>,
    /// What only the person can do, still open: the brief's "for you".
    pub for_you: Vec<OpenItem>,
    restated: Vec<(Uuid, bool)>,
    pub told: HashMap<Uuid, Told>,
    /// Suggestions its quiet check-ins made, or notes on finished
    /// sessions, wait for the next update.
    noted: bool,
}

impl Batch {
    /// What of it goes right away by the cadence `c` (`quiet`: in the
    /// quiet hours): the sessions whose news is of a kind that comes at
    /// once, the decisions that do, and what needs the person when it
    /// does. What goes in the roundup, or only when asked, is left out;
    /// what each session's account said is kept whole, so it is all told.
    ///
    /// What waits keeps what was last told of it (`was`), so the roundup
    /// still finds it changed: a new next step or wait is said then.
    fn right_away(self, c: &Cadence, quiet: bool, was: &HashMap<Uuid, Told>) -> Batch {
        let now = |k: EventKind| cadence::route(k, c, quiet) == Route::RightAway;
        // What comes next, or what it waits on, changed: progress.
        let kind_of = |s: &Changed| {
            let new: Vec<&Done> = s.new.iter().collect();
            match new.is_empty() {
                true => EventKind::Progress,
                false => write::kind_of(&new, &[]),
            }
        };
        let mut told = self.told;
        let (sessions, waits): (Vec<Changed>, Vec<Changed>) =
            self.sessions.into_iter().partition(|s| now(kind_of(s)));
        // Held for the roundup: still changed when it comes. Said only if
        // asked: moved on, so it neither piles up nor comes out later.
        for s in waits
            .iter()
            .filter(|s| cadence::route(kind_of(s), c, quiet) != Route::Ask)
        {
            match was.get(&s.id) {
                Some(t) => told.insert(s.id, t.clone()),
                None => told.remove(&s.id),
            };
        }
        let decisions: Vec<BossDecision> = self
            .decisions
            .into_iter()
            .filter(|d| match d.kind {
                DecisionKind::LeftAlone | DecisionKind::Undone => now(EventKind::Routine),
                DecisionKind::Escalated => now(EventKind::NeedsYou),
                DecisionKind::Acted if update::is_close(d) => now(EventKind::Finished),
                DecisionKind::Acted => {
                    cadence::decision_route(d.important.is_some(), c, quiet) == Route::RightAway
                }
            })
            .collect();
        let needs = now(EventKind::NeedsYou);
        let kept = match now(EventKind::Finished) {
            true => self.kept,
            false => Vec::new(),
        };
        let (waiting, for_you, restated) = match needs {
            true => (self.waiting, self.for_you, self.restated),
            false => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut change = sessions
            .iter()
            .map(|s| match s.new.iter().any(update::is_result) {
                true => Change::Result,
                false => Change::Small,
            })
            .max()
            .unwrap_or(Change::None);
        if !decisions.is_empty() || !kept.is_empty() || (self.noted && needs) {
            change = Change::Result;
        }
        if !restated.is_empty() {
            change = change.max(Change::Small);
        }
        Batch {
            change,
            sessions,
            decisions,
            kept,
            waiting,
            for_you,
            restated,
            told,
            noted: self.noted && needs,
        }
    }
}

impl CrewHub {
    /// Sets who writes the updates' paragraphs; with none, they are
    /// written plainly.
    pub(crate) fn set_update_writer(&self, writer: Arc<dyn UpdateWriter>) {
        *self.update_writer.lock() = Some(writer);
    }

    fn minutes(&self, n: u32) -> Duration {
        Duration::from_std(self.timing.minute).unwrap_or_default() * n as i32
    }

    /// What `service`'s next update would say, from `state`, at `now`.
    pub(super) async fn batch(
        &self,
        service: &AgentService,
        o: &Oversight,
        state: &UpdateState,
        now: DateTime<Utc>,
        away: Option<DateTime<Utc>>,
    ) -> Batch {
        let boss = service.agent_id.as_str();
        let person = self.person(Some(service));
        let seen = self.desk.seen_by(&o.scope);
        let views: Vec<_> = self
            .desk
            .list()
            .into_iter()
            .filter(|v| v.state != SessionState::Closed)
            .filter(|v| seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
            .filter(|v| !workforce_desk::ledger::agent_turn(&v.cwd, &v.name))
            .filter(|v| !self.desk.hub_internal(v.id))
            .collect();
        let ids: Vec<Uuid> = views.iter().map(|v| v.id).collect();
        let accounts = self
            .desk
            .session_accounts_of(&ids)
            .await
            .unwrap_or_else(|e| {
                tracing::info!(error = %e, "the sessions' accounts couldn't be read for an update");
                Vec::new()
            });
        let mut change = Change::None;
        let mut sessions = Vec::new();
        let mut told = state.told.clone();
        for v in &views {
            let Some(a) = accounts.iter().find(|a| a.session == v.id) else {
                continue;
            };
            let was = state.told.get(&v.id);
            let mut c = update::changed(was, a, state.since);
            let mut new: Vec<Done> = update::done_since(was, a, state.since)
                .into_iter()
                .cloned()
                .collect();
            // A catch-up says all that was done while the person was away,
            // told in an update they didn't read or not.
            if let Some(from) = away {
                let all = update::done_since(None, a, from.max(state.since));
                if !all.is_empty() {
                    c = c.max(match all.iter().any(|d| update::is_result(d)) {
                        true => Change::Result,
                        false => Change::Small,
                    });
                    new = all.into_iter().cloned().collect();
                }
            }
            told.insert(v.id, Told::of(a));
            if c == Change::None {
                continue;
            }
            change = change.max(c);
            let work = self.work_of(v, Some(a), &new, person.as_deref()).await;
            sessions.push(Changed {
                id: v.id,
                name: v.label().to_string(),
                repo: repo_of(v, &new),
                // What it waits on is said only when that is new.
                account: update::as_told(was, a),
                new,
                work,
            });
        }
        // Since what was last said, or left for the roundup.
        let mut last = state
            .last_update_at
            .unwrap_or(state.since)
            .max(state.routed_at.unwrap_or(state.since));
        if let Some(from) = away {
            last = last.min(from);
        }
        let store = self.desk.store();
        let composed = o.updates.enabled;
        let decisions: Vec<BossDecision> = store
            .decisions_since(boss, last, 500)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.at > last && d.at <= now && d.undone_at.is_none())
            // Without the composer, an answer to a session was said at once.
            .filter(|d| composed || d.need != Need::Question)
            .collect();
        let kept: Vec<String> = match composed {
            true => store
                .items_closed_since(boss, last, 200)
                .await
                .unwrap_or_default()
                .iter()
                .filter(|i| i.closed_at.is_some_and(|at| at > last && at <= now))
                .filter_map(super::items::kept_line)
                .collect(),
            false => Vec::new(),
        };
        let gap = self.answer_gap(o);
        let open: Vec<OpenItem> = store
            .items_not_closed(Some(boss))
            .await
            .unwrap_or_default()
            .into_iter()
            // The person in that session now: theirs, nothing said of it,
            // nor counted as said again.
            .filter(|i| {
                !matches!(i.kind, OpenKind::Ask | OpenKind::Confirm)
                    || i.session.is_none_or(|s| !self.person_in_for(s, i.opened_at, gap))
            })
            .collect();
        let restated: Vec<(Uuid, bool)> = match composed {
            true => self
                .due_again(&open, o, now)
                .into_iter()
                .map(|(i, _, park)| (i.id, park))
                .collect(),
            false => Vec::new(),
        };
        // What waits on the person is said when it is new: opened or changed
        // since the last update, or due to be said again. One they were
        // told of and that hasn't moved is its own card, not news again.
        let new_since =
            |i: &OpenItem| i.changed_at > last || restated.iter().any(|(r, _)| *r == i.id);
        let for_you: Vec<OpenItem> = open
            .iter()
            .filter(|i| i.kind == OpenKind::Todo && i.state == OpenState::Open && i.said())
            .filter(|i| new_since(i))
            .cloned()
            .collect();
        let waiting: Vec<OpenItem> = open
            .into_iter()
            .filter(|i| matches!(i.kind, OpenKind::Ask | OpenKind::Confirm))
            .filter(|i| i.said() && i.state == OpenState::Open)
            .filter(|i| new_since(i))
            .collect();
        let quiet = self
            .quiet_made
            .lock()
            .get(boss)
            .is_some_and(|m| !m.is_empty())
            || self.has_closing_notes(boss);
        if !decisions.is_empty() || !kept.is_empty() || quiet {
            change = Change::Result;
        }
        let noted = quiet;
        if !restated.is_empty() {
            change = change.max(Change::Small);
        }
        Batch {
            change,
            sessions,
            decisions,
            kept,
            waiting,
            for_you,
            restated,
            told,
            noted,
        }
    }

    /// The session paragraphs and decision lines of `b`: the cheap
    /// model's in the boss's voice when it is on and may be asked, else
    /// plainly from the rows; any it leaves out (or writes out of bounds)
    /// plainly too.
    pub(super) async fn composed(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
    ) -> (
        Vec<UpdateSession>,
        Vec<DecisionLine>,
        Option<String>,
        HashMap<Uuid, write::Written>,
    ) {
        let mut written = update::Composed::default();
        let writer = self.update_writer.lock().clone();
        // Only what it did is worth the model's words, the newest first: the
        // rest read as their rows.
        let mut acted: Vec<BossDecision> = b
            .decisions
            .iter()
            .rev()
            .filter(|d| d.kind == DecisionKind::Acted)
            // Said in the closed line, in its own words.
            .filter(|d| !update::is_close(d))
            .cloned()
            .collect();
        // The important ones first: a roundup lists them in the words
        // written for them.
        acted.sort_by_key(|d| d.important.is_none());
        acted.truncate(update::COMPOSED_DECISIONS);
        acted.sort_by_key(|d| d.at);
        let something = !b.sessions.is_empty() || !acted.is_empty();
        if let (true, Some(writer), true) = (o.updates.enabled, writer, something) {
            let billing = writer.billing(boss).await;
            if self.desk.update_take(billing).await.unwrap_or(false) {
                let input: Vec<(Uuid, String, &Account, Vec<&Done>)> = b
                    .sessions
                    .iter()
                    .map(|s| (s.id, s.context(), &s.account, s.new.iter().collect()))
                    .collect();
                let mut user = update::composer_input(&input);
                let works: Vec<(Uuid, &Work)> =
                    b.sessions.iter().map(|s| (s.id, &s.work)).collect();
                user.push_str(&update::composer_work(&works));
                user.push_str(&update::composer_decisions(&acted));
                match writer
                    .write(boss, owner, update::COMPOSER_SYSTEM, &user)
                    .await
                {
                    Ok(answer) => match update::parse_composed(&answer) {
                        Some(got) => written = got,
                        None => tracing::info!(
                            "the update's writer answered out of form; written plainly"
                        ),
                    },
                    Err(e) => {
                        tracing::info!(error = %format!("{e:#}"), "the update's writer failed; written plainly")
                    }
                }
                // Over its bounds, or against the writing contract (an id
                // alone, outside brackets): asked once more for those
                // fields alone, then whatever is still long is clipped and
                // whatever still breaks it is written plainly.
                let long = update::too_long(&written);
                let broken = Self::breaking(b, &written);
                if !(long.is_empty() && broken.is_empty())
                    && self.desk.update_take(billing).await.unwrap_or(false)
                {
                    user.push_str(&update::reask_for(&long, &broken));
                    match writer
                        .write(boss, owner, update::COMPOSER_SYSTEM, &user)
                        .await
                        .map(|a| update::parse_composed(&a))
                    {
                        Ok(Some(again)) => {
                            written = update::merge_composed(std::mem::take(&mut written), again)
                        }
                        Ok(None) => tracing::info!(
                            "the update's writer answered a re-ask out of form; clipped"
                        ),
                        Err(e) => {
                            tracing::info!(error = %format!("{e:#}"), "the update's writer failed a re-ask; clipped")
                        }
                    }
                }
            }
        }
        let sessions = b
            .sessions
            .iter()
            .map(|s| {
                let new: Vec<&Done> = s.new.iter().collect();
                UpdateSession {
                    session_id: s.id.to_string(),
                    name: s.name.clone(),
                    repo: s.repo.clone(),
                    purpose: s.account.purpose.trim().to_string(),
                    text: written
                        .sessions
                        .remove(&s.id)
                        .map(|t| workforce_desk::crew::words(&t, update::SESSION_WORDS))
                        .unwrap_or_else(|| update::plain_session(&new, &s.account)),
                    refs: update::refs_of(&new),
                }
            })
            .collect();
        let decisions = update::decision_lines(&b.decisions, &written.decisions);
        let headline = written.headline.as_deref().and_then(update::headline_line);
        (sessions, decisions, headline, written.said)
    }

    /// What of `c`'s items for `b`'s sessions breaks the writing contract
    /// ([`write::breaches`]), said for the composer to write again.
    fn breaking(b: &Batch, c: &update::Composed) -> Vec<String> {
        let mut out = Vec::new();
        for s in &b.sessions {
            let Some(w) = c.said.get(&s.id) else {
                continue;
            };
            let probe = |headline: &str, body: &str| {
                write::breaches(&Said {
                    kind: write::EventKind::Progress,
                    headline: headline.to_string(),
                    body: body.to_string(),
                    ids: vec![],
                    not_checked: None,
                    session: Some(s.work.link()),
                    did: vec![],
                    at: None,
                    decisions: None,
                })
            };
            if let Some(h) = w.headline.as_deref() {
                let said: Vec<String> = probe(h, "").iter().map(write::Breach::said).collect();
                if !said.is_empty() {
                    out.push(format!("session {} headline ({})", s.id, said.join("; ")));
                }
            }
            // What follows is put right as it is kept (its ids lifted to
            // the brackets): nothing to ask again.
        }
        out
    }

    /// What session `v`'s work is, from facts: what it is for (its
    /// account's purpose, else its goal), the tickets on the board it is
    /// given or linked to, with their titles, and the PRs it names (`new`),
    /// with the titles `crew.github_pr` read of them.
    pub(super) async fn work_of(
        &self,
        v: &workforce_desk::model::SessionView,
        a: Option<&Account>,
        new: &[Done],
        person: Option<&str>,
    ) -> Work {
        let store = self.desk.store();
        let repo = repo_of(v, new);
        let mut tickets = store.session_tickets(v.id).await.unwrap_or_default();
        for t in store.linked_tickets(&[v.id]).await.unwrap_or_default() {
            if !tickets.iter().any(|k| k.id == t.id) {
                tickets.push(t);
            }
        }
        let keys = store
            .ticket_keys(&tickets.iter().map(|t| t.id).collect::<Vec<_>>())
            .await
            .unwrap_or_default();
        let mut titled: Vec<Titled> = tickets
            .iter()
            .filter_map(|t| {
                Some(Titled {
                    kind: workforce_desk::account::RefKind::Ticket,
                    id: keys.get(&t.id)?.clone(),
                    title: Some(t.title.clone()).filter(|t| !t.trim().is_empty()),
                })
            })
            .collect();
        for r in new.iter().flat_map(|d| &d.refs) {
            use workforce_desk::account::RefKind;
            if !matches!(r.kind, RefKind::Pr | RefKind::Ticket)
                || titled.iter().any(|t| t.id.eq_ignore_ascii_case(&r.id))
                || write::id_kind(&r.id) != Some(r.kind)
            {
                continue;
            }
            let title = match r.kind {
                RefKind::Pr => person.and_then(|p| {
                    super::github::pr_title(p, r.url.as_deref(), repo.as_deref(), &r.id)
                }),
                _ => None,
            };
            titled.push(Titled {
                kind: r.kind,
                id: r.id.trim().to_string(),
                title,
            });
        }
        let goal = a
            .map(|a| a.purpose.trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| v.goal.trim().to_string());
        Work {
            session_id: v.id.to_string(),
            name: v.label().to_string(),
            closed: v.state == SessionState::Closed,
            goal,
            titled,
        }
    }

    /// `b` written as an update for `owner`: its extras and its text.
    async fn written(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
        now: DateTime<Utc>,
    ) -> (MentionedUpdate, String, bool) {
        let (sessions, decisions, headline, wrote) = self.composed(boss, owner, o, b).await;
        let tz = self.desk.timezone();
        let composed_headline = headline.is_some();
        let full = self.level_now(boss, o).0 == workforce_common::oversight::Level::Full;
        let footer = match self.desk.cheap_today().await {
            Ok(today) => update::footer(&today),
            Err(_) => None,
        };
        // Each group of the brief is named by its session and repository,
        // so a line read alone still says what it is about.
        let about: std::collections::HashSet<Uuid> = b
            .decisions
            .iter()
            .filter_map(|d| d.session)
            .chain(b.for_you.iter().filter_map(|i| i.session))
            .collect();
        let names: HashMap<Uuid, String> = self
            .desk
            .list()
            .into_iter()
            .filter(|v| about.contains(&v.id))
            .map(|v| {
                // Named as its paragraph names it, so one session is one
                // subject however its repository was found.
                let named = match b.sessions.iter().find(|s| s.id == v.id) {
                    Some(s) => s.context(),
                    None => update::subject(v.label(), repo_of(&v, &[]).as_deref()),
                };
                (v.id, named)
            })
            .collect();
        let (brief, more_items) = update::brief(&b.decisions, &decisions, &b.for_you, &names);
        // Said as a teammate says it: one item per piece of work, the
        // composer's words where they keep the contract, else plainly.
        let mut said: Vec<Said> = Vec::new();
        for s in &b.sessions {
            let new: Vec<&Done> = s.new.iter().collect();
            let did = update::did_items(&b.decisions, &decisions, Some(s.id), &tz);
            let kind = write::kind_of(&new, &did);
            let plain = write::plain_said(
                &s.work,
                &new,
                &s.account.next,
                s.account.waiting_on.as_deref(),
                did,
                kind,
            );
            said.push(match wrote.get(&s.id) {
                Some(w) => write::said_from(plain, w, &s.work),
                None => plain,
            });
        }
        // The sessions it acted on that did nothing new themselves.
        let views = self.desk.list();
        let mut acted_on: Vec<Uuid> = Vec::new();
        for id in b.decisions.iter().filter_map(|d| d.session) {
            if !acted_on.contains(&id) && !b.sessions.iter().any(|s| s.id == id) {
                acted_on.push(id);
            }
        }
        for id in &acted_on {
            let did = update::did_items(&b.decisions, &decisions, Some(*id), &tz);
            let Some(v) = views
                .iter()
                .find(|v| v.id == *id)
                .filter(|_| !did.is_empty())
            else {
                continue;
            };
            let work = self.work_of(v, None, &[], Some(owner)).await;
            let kind = write::kind_of(&[], &did);
            said.push(write::plain_said(&work, &[], "", None, did, kind));
        }
        // The finished sessions it closed, would close, or left open: one
        // line each.
        let notes = self.closing_notes(boss, now);
        let of = |kind: update::ClosedKind| -> Vec<update::ClosedSession> {
            notes
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, s)| s.clone())
                .collect()
        };
        let closed: Vec<update::ClosedLine> = update::closed_line(&b.decisions)
            .into_iter()
            .chain(update::noted_line(
                update::ClosedKind::Suggested,
                of(update::ClosedKind::Suggested),
            ))
            .chain(update::noted_line(
                update::ClosedKind::KeptOpen,
                of(update::ClosedKind::KeptOpen),
            ))
            .collect();
        let mut u = MentionedUpdate {
            sessions,
            decisions,
            kept: b.kept.clone(),
            waiting: b
                .waiting
                .iter()
                .map(|i| WaitingItem::of(i, now, full))
                .collect(),
            needs_you: b
                .waiting
                .iter()
                .map(|i| NeedsYou::of(i, now, full))
                .collect(),
            footer,
            headline: String::new(),
            brief,
            more_items,
            closed,
            said,
            roundup: None,
        };
        // Not written: one piece of work's own headline, else in counts.
        let headline = headline
            .or_else(|| match u.said.as_slice() {
                [one] if b.for_you.is_empty() && b.waiting.is_empty() => {
                    update::headline_line(&one.headline)
                }
                _ => None,
            })
            .unwrap_or_else(|| update::plain_headline(&u, b.for_you.len()));
        // Never a line without its session: one about one session says it.
        u.headline = update::situated(&u, &headline, b.for_you.len());
        let decided: Vec<String> = b
            .decisions
            .iter()
            .filter(|d| !update::is_close(d))
            .map(BossDecision::said)
            .collect();
        let text = update::render(&u, &decided);
        (u, text, composed_headline)
    }

    /// Looks at every boss's update once: sends one written but not yet
    /// gone, starts gathering on news, and sends what is due.
    pub(crate) async fn look_at_updates(&self) {
        for (service, o) in self.overseers() {
            let boss = service.agent_id.to_string();
            // One look at a boss's update at a time: a wake-up's check-in
            // may be writing it.
            let Some(_one) = Updating::begin(&self.updating, &boss) else {
                continue;
            };
            // What the person settled in a session themselves is no longer
            // theirs to answer here.
            self.settle_in_sessions(&boss).await;
            if let Err(e) = self.update_of(&service, &o).await {
                tracing::warn!(agent = %service.agent_id, error = %format!("{e:#}"), "a boss's update failed");
            }
        }
    }

    async fn update_of(&self, service: &Arc<AgentService>, o: &Oversight) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        let Some(owner) = self.person(Some(service)) else {
            return Ok(());
        };
        let now = Utc::now();
        let store = self.desk.store();
        let state = store.update_state(boss, now).await?;
        let dm = self.dm_look(service, &owner, o);
        // While the person types to it, or its reply is being written,
        // nothing of this goes out: the next look sends it.
        let held = self.update_held(boss, dm.session.as_deref());
        let mut state = state;
        if let Some(p) = state.pending.clone() {
            // An update that didn't go before the person went away (a
            // failed send, a restart): the catch-up covers it, rather than
            // a second message for one return.
            let away = state.away_since.is_some() || dm.away.is_some();
            if away && p.away.is_none() && p.sent.is_none() && !p.handed {
                store.update_unpend(boss).await?;
                state.pending = None;
            } else if held && p.sent.is_none() {
                self.poke_at_release(boss);
                return Ok(());
            } else {
                return self.resume(service, &owner, o, &state, p).await;
            }
        }
        // Away: what changes gathers for their return. Back: one catch-up.
        let away = match (state.away_since, dm.away) {
            (None, Some(since)) => {
                store.update_away(boss, since).await?;
                true
            }
            (Some(_), Some(_)) => true,
            (Some(since), None) if !held => {
                return self.catch_up(service, &owner, o, &state, since).await;
            }
            (Some(_), None) => {
                self.poke_at_release(boss);
                return Ok(());
            }
            (None, None) => false,
        };
        let c = o.updates.cadence;
        let tz = self.desk.timezone();
        // The roundup, when its time has come (or the person asked for one
        // now), while they are here and nothing is being written to them:
        // what happened since the last, as one message.
        if !away && !held {
            // Since the last roundup; with none, since what was last said:
            // never all since the boss began. With roundups never, the one
            // that comes as the quiet hours end says only what they held.
            let said = state.last_update_at.unwrap_or(state.since);
            let last = match c.roundup {
                RoundupEvery::Never => said.max(state.roundup_at.unwrap_or(said)),
                _ => state.roundup_at.unwrap_or(said),
            };
            let asked = self.roundup_asked.lock().remove(boss);
            if asked || cadence::roundup_due(&c, last, now, &tz) {
                if let Some(p) = self
                    .roundup(service, &owner, o, &state, last, now, asked)
                    .await?
                {
                    return self.resume(service, &owner, o, &state, p).await;
                }
            }
        }
        let all = self.batch(service, o, &state, now, None).await;
        let something = all.change != Change::None;
        let b = all.right_away(&c, cadence::quiet_now(&c, now, &tz), &state.told);
        if b.change == Change::None && something && state.gather_since.is_none() {
            // All of it waits for the roundup, or for the person's asking:
            // routed, and nothing goes now.
            store.update_routed(boss, now, &b.told).await?;
            return Ok(());
        }
        let gather_since = match (b.change, state.gather_since) {
            (Change::None, None) => return Ok(()),
            (Change::None, Some(_)) => {
                // What was gathered no longer says anything (undone, or
                // told another way): nothing to send.
                store.update_regather(boss, &b.told).await?;
                return Ok(());
            }
            (c, since) => {
                let result = c == Change::Result;
                if since.is_none() || (result && !state.result) {
                    store.update_gather(boss, now, result).await?;
                }
                since.unwrap_or(now)
            }
        };
        if away || held {
            self.poke_at_release(boss);
            return Ok(());
        }
        let read = service
            .sessions
            .desk_read(&owner, DESK_CHANNEL)
            .ok()
            .flatten()
            .map(|r| r.at);
        let unread = update::streak(state.unread, state.last_update_at, read);
        let every = self.minutes(o.updates.every_minutes);
        let max_wait = self.minutes(o.updates.max_wait_minutes);
        let gap = update::interval(every, max_wait, unread);
        let result = state.result || b.change == Change::Result;
        if !update::due(
            Some(gather_since),
            result,
            state.last_update_at,
            now,
            (every, max_wait, gap),
        ) {
            return Ok(());
        }
        let (level, _) = self.level_now(boss, o);
        let p = self.pend(boss, &owner, o, &b, now, !level.acts()).await?;
        self.resume(service, &owner, o, &state, p).await
    }

    /// `boss`'s roundup of what happened since `since` (shape C), written
    /// and kept as its pending update before anything is sent; none when it
    /// would say nothing but routine, which is then skipped. Each section
    /// only when it says something: what is up to the person, what it
    /// decided without them that is important (or every one, as they
    /// chose), what finished, what is worth knowing, what is still going;
    /// the rest counted as routine. What came right away is folded in.
    pub(super) async fn roundup(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
        asked: bool,
    ) -> eyre::Result<Option<Pending>> {
        let boss = service.agent_id.as_str();
        let c = o.updates.cadence;
        // What a held card puts to the person comes with that card.
        let carded = self
            .desk
            .store()
            .cards_held_items(boss)
            .await
            .unwrap_or_default();
        let b = self.batch(service, o, state, now, Some(since)).await;
        let (mut u, _, composed) = self.written(boss, owner, o, &b, now).await;
        let mut lines: Vec<(SectionKey, Said)> = Vec::new();
        // What the person can answer or lift, each with its session.
        for n in &u.needs_you {
            if n.item_id
                .parse::<Uuid>()
                .is_ok_and(|id| carded.contains(&id))
            {
                continue;
            }
            let session = n
                .session_id
                .as_deref()
                .and_then(|s| s.parse::<Uuid>().ok())
                .and_then(|s| self.desk.view(s).ok())
                .map(|v| write::SessionLink {
                    session_id: v.id.to_string(),
                    name: v.label().to_string(),
                    closed: v.state == SessionState::Closed,
                });
            lines.push((
                SectionKey::UpToYou,
                Said {
                    kind: EventKind::NeedsYou,
                    headline: n.question.clone(),
                    body: n.not_blocking.clone().unwrap_or_default(),
                    ids: vec![],
                    not_checked: None,
                    session,
                    did: vec![],
                    at: None,
                    decisions: None,
                },
            ));
        }
        // What it decided without them, listed as they chose: one entry a
        // session (and one for what is about none), what did something that
        // matters first, its important decisions under it, each with its
        // Undo; the routine ones a line a session and kind, behind the
        // routine count (listed in their session's entry when they asked
        // for every one).
        let views = self.desk.list();
        let tz = self.desk.timezone();
        let label = |id: Uuid| {
            views
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.label().to_string())
        };
        let link = |id: Option<Uuid>| {
            let v = views.iter().find(|v| Some(v.id) == id)?;
            Some(write::SessionLink {
                session_id: v.id.to_string(),
                name: v.label().to_string(),
                closed: v.state == SessionState::Closed,
            })
        };
        let told = routine::told(&b.decisions, c.decisions, &label, &tz);
        let listed = told
            .listed
            .iter()
            .copied()
            .chain(told.listed_runs.iter().flat_map(|r| r.rows.iter().copied()));
        for e in routine::by_session(listed, &label) {
            let said = e.said(&u.decisions, link(e.session), &tz);
            if !said.did.is_empty() {
                lines.push((SectionKey::Decided, said));
            }
        }
        // Each piece of work, in the section its kind is said in, unless the
        // person hears of that kind only when they ask.
        for s in &u.said {
            if cadence::route(s.kind, &c, false) == Route::Ask {
                continue;
            }
            let mut s = s.clone();
            // What it decided is said above, or behind the routine count.
            s.did.clear();
            lines.push((SectionKey::of(s.kind), s));
        }
        for k in &b.kept {
            lines.push((
                SectionKey::Finished,
                Said {
                    kind: EventKind::Finished,
                    headline: k.trim_start_matches('✓').trim().to_string(),
                    body: String::new(),
                    ids: vec![],
                    not_checked: None,
                    session: None,
                    did: vec![],
                    at: None,
                    decisions: None,
                },
            ));
        }
        let runs = told
            .routine
            .iter()
            .map(|r| r.said(&u.decisions, link(r.session)))
            .collect();
        let r = Roundup::of(since, lines, told.routine_count(), runs);
        let nothing = r.is_empty() && u.closed.is_empty();
        if nothing && !asked {
            self.desk.store().update_rounded(boss, now).await?;
            return Ok(None);
        }
        // Its headline is what matters most, in a line: the composer's, or
        // in counts; asked for with nothing to say, it says so.
        if nothing {
            let at = since.with_timezone(&self.desk.timezone()).format("%H:%M");
            u.headline = format!("Nothing new since {at}.");
        } else if !composed {
            u.headline = r.plain_headline();
        }
        let mut text = match nothing {
            true => u.headline.clone(),
            false => cadence::render(&u.headline, &r),
        };
        for l in &u.closed {
            text.push_str(&format!("\n\n{}", l.text));
        }
        u.roundup = Some(r);
        let p = Pending {
            id: Uuid::new_v4(),
            text,
            update: u,
            told: b.told.clone(),
            restated: vec![],
            cutoff: Some(now),
            handed: false,
            sent: None,
            away: None,
        };
        self.desk.store().update_pend(boss, &p).await?;
        Ok(Some(p))
    }

    /// `b` written and kept as `boss`'s pending update, before anything is
    /// sent: `handed` to a check-in at Watch and Suggest.
    async fn pend(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
        now: DateTime<Utc>,
        handed: bool,
    ) -> eyre::Result<Pending> {
        let (u, text, _) = self.written(boss, owner, o, b, now).await;
        let p = Pending {
            id: Uuid::new_v4(),
            text,
            update: u,
            told: b.told.clone(),
            restated: b.restated.clone(),
            cutoff: Some(now),
            handed,
            sent: None,
            away: None,
        };
        self.desk.store().update_pend(boss, &p).await?;
        Ok(p)
    }

    /// Sends the pending `p`, as it was written: handed to a check-in while
    /// one may run (it ran already when its check-in answered), else as an
    /// update of its own.
    async fn resume(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        p: Pending,
    ) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        // Its check-in answered it already: only what follows is left.
        if p.handed && p.sent.is_some() {
            return self.told(service, owner, state, &p).await;
        }
        if p.handed {
            // A check-in that answered it before a restart was its post.
            let answered = self
                .desk
                .store()
                .latest_check_in(boss)
                .await
                .ok()
                .flatten()
                .filter(|c| c.prompt.contains(&marker(&p)))
                .and_then(|c| c.reply);
            if let Some(reply) = answered {
                return self.answered(service, owner, state, &p, &reply).await;
            }
            if matches!(self.budget(boss, o).await, Ok(Budget::Open)) {
                return self.in_check_in(service, owner, o, state, p).await;
            }
            // The check-ins are spent: the update goes out on its own.
        }
        self.send(service, owner, state, p).await
    }

    /// The prompt's part that hands `p` to a check-in, found again by its
    /// id after a restart.
    fn handing(p: &Pending) -> String {
        format!(
            "Your update to the person is due ({}). Here is what changed since you last told them, each session's \
             paragraph already written:\n{}\n\n\
             Give the person this update in your message: keep each session's paragraph and every line that \
             starts \"I decided\", \"Still waiting on you\" or says what you kept, word for word, and add what you suggest.",
            marker(p),
            super::watcher::reported(&p.text, "your update"),
        )
    }

    /// What a wake-up's check-in at Watch or Suggest also carries: the
    /// boss's update, written now from what changed, as its one post. None
    /// when nothing changed, or another look is writing it.
    pub(super) async fn update_for_wake(
        &self,
        service: &AgentService,
        o: &Oversight,
    ) -> Option<(Pending, String, Updating<'_>)> {
        let boss = service.agent_id.as_str();
        let one = Updating::begin(&self.updating, boss)?;
        let owner = self.person(Some(service))?;
        let now = Utc::now();
        let state = self.desk.store().update_state(boss, now).await.ok()?;
        // Away, it gathers for their catch-up instead.
        if state.away_since.is_some() {
            return None;
        }
        let p = match state.pending.clone() {
            Some(p) if p.handed && p.sent.is_none() => p,
            Some(_) => return None,
            None => {
                let b = self.batch(service, o, &state, now, None).await;
                if b.change == Change::None {
                    return None;
                }
                self.pend(boss, &owner, o, &b, now, true).await.ok()?
            }
        };
        let handing = Self::handing(&p);
        Some((p, handing, one))
    }

    /// A wake-up's check-in carried `p` and answered `reply`: told.
    pub(super) async fn update_answered(
        &self,
        service: &Arc<AgentService>,
        p: &Pending,
        reply: &str,
    ) {
        let boss = service.agent_id.as_str();
        let Some(owner) = self.person(Some(service)) else {
            return;
        };
        let state = match self.desk.store().update_state(boss, Utc::now()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "a boss's update state couldn't be read");
                return;
            }
        };
        if let Err(e) = self.answered(service, &owner, &state, p, reply).await {
            tracing::warn!(error = %format!("{e:#}"), "marking a check-in's update told failed");
        }
    }

    /// Hands `p` to a check-in of its own at Watch or Suggest: its message,
    /// with its suggestions, is the update.
    async fn in_check_in(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        p: Pending,
    ) -> eyre::Result<()> {
        let (level, _) = self.level_now(service.agent_id.as_str(), o);
        let prompt = format!(
            "{CHECK_IN} {} (You oversee the crew at level {}.) {}",
            Self::handing(&p),
            level.as_str(),
            match self.unbound_given(service.agent_id.as_str(), o) {
                true => Self::boss_hint(o),
                false => Self::level_hint(level, o),
            },
        );
        // A check-in like any other: it counts against the hour's.
        if let Some(reply) = self
            .check_in_on(service, owner, "wake", &prompt, 0, None, false)
            .await
        {
            self.answered(service, owner, state, &p, &reply).await?;
        }
        Ok(())
    }

    /// A check-in answered `reply` for `p`: what of its decisions, what it
    /// kept and what still waits the reply left out goes out as the rows
    /// say it, so none is lost to its wording; then told.
    async fn answered(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        state: &UpdateState,
        p: &Pending,
        reply: &str,
    ) -> eyre::Result<()> {
        let decided: Vec<String> = self
            .desk
            .store()
            .decisions_of(
                &p.update
                    .decisions
                    .iter()
                    .filter_map(|d| d.id.parse().ok())
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap_or_default()
            .iter()
            .map(BossDecision::said)
            .collect();
        // Answered: kept so first, so a restart from here on neither hands
        // it to another check-in nor posts what it left out twice.
        let mut p = p.clone();
        p.sent = Some(format!("check-in {}", marker(&p)));
        self.desk
            .store()
            .update_pend(service.agent_id.as_str(), &p)
            .await?;
        // The check-in's turn is the crew's housekeeping, never on the
        // Floor: what it said is, briefly.
        if let Some(dm) = service.sessions.find(DESK_CHANNEL, owner).ok().flatten() {
            self.on_floor(service, owner, &dm.id, &p);
        }
        let missing = update::missing_lines(&p, &decided, reply);
        if !missing.is_empty() {
            let target =
                DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
            let delivery =
                crate::desk_surface::Delivery::text(DeliveryKind::Update, missing.join("\n\n"));
            if let Err(e) = crate::desk_surface::deliver(service, owner, &target, delivery).await {
                tracing::warn!(error = %e, "posting what a check-in's update left out failed");
            }
        }
        self.told(service, owner, state, &p).await
    }

    /// Sends `p` as `service`'s update, once: when a restart finds it
    /// already in the DM, it is only marked told; one that couldn't go is
    /// kept, and sent as it is on a later look.
    pub(super) async fn send(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        state: &UpdateState,
        mut p: Pending,
    ) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        let id = p.id.to_string();
        let found = p.sent.clone().or_else(|| {
            service
                .sessions
                .find(DESK_CHANNEL, owner)
                .ok()
                .flatten()
                .and_then(|s| {
                    s.messages
                        .iter()
                        .rev()
                        .find(|m| {
                            m.desk.as_ref().and_then(|d| d.request_id.as_deref()) == Some(&id)
                        })
                        .map(|m| m.id.clone())
                })
        });
        let mut extras = Extras {
            update: Some(p.update.clone()),
            away: p.away.clone(),
            suggestions: self.quiet_suggestions(boss, &p),
            ..Default::default()
        };
        match found {
            Some(m) => {
                // The line on the directives it went with stays with it.
                extras.directives_open = self
                    .desk
                    .store()
                    .extras(std::slice::from_ref(&m))
                    .await
                    .ok()
                    .and_then(|mut e| e.remove(&m))
                    .and_then(|e| e.directives_open);
                self.keep_extras_of(&m, boss, &extras).await
            }
            None => {
                // Going out now: at a quiet moment, at most once a day, one
                // line on the directives still open.
                if let Some((_, o)) = self.overseer_named(boss) {
                    extras.directives_open = self.directives_line(boss, &o, &p).await;
                }
                let kind = match p.away {
                    Some(_) => DeliveryKind::Away,
                    None => DeliveryKind::Update,
                };
                let mut delivery = crate::desk_surface::Delivery::text(kind, p.text.clone());
                delivery.request_id = Some(id);
                // Its extras kept first: the update is read whole the
                // moment it is in the DM.
                let delivered = self
                    .deliver_with_extras(service, owner, delivery, &extras)
                    .await?;
                self.on_floor(service, owner, &delivered.session_id, &p);
                if let Some(n) = &extras.directives_open {
                    // Said: the next is a day off.
                    if let Err(e) = self
                        .desk
                        .store()
                        .directive_nudged(workforce_desk::rooms::YOU, &n.boss_scope)
                        .await
                    {
                        tracing::warn!(error = %format!("{e:#}"), "a directives line wasn't kept as said");
                    }
                }
                let message = delivered.message_id;
                // Sent: kept so, before anything else, so a restart from
                // here on finds it whatever the DM did since.
                p.sent = Some(message);
                self.desk.store().update_pend(boss, &p).await?;
            }
        }
        self.told(service, owner, state, &p).await
    }

    /// The Floor told of `p` as it went to `owner`'s DM `session_id`: the
    /// boss's purpose and where things stand, from its brief, in plain
    /// words and no ids; what was settled since and a session the person
    /// is in now left out. Never the update's prose.
    fn on_floor(&self, service: &AgentService, owner: &str, session_id: &str, p: &Pending) {
        #[cfg(feature = "postgres")]
        if let Some(log) = crate::floor::turns() {
            let now = Utc::now();
            let hidden = |s: &str| s.parse::<Uuid>().is_ok_and(|s| self.person_in(s, now));
            use workforce_desk::update_brief as brief;
            let shown = brief::shown(&p.update, &hidden);
            // All of it about sessions the person is in: nothing is said.
            let hid = brief::hides(&p.update, &shown);
            if !brief::says_anything(&shown) && (p.away.is_none() || hid) {
                return;
            }
            // A catch-up's own headline, unless it was written over what
            // is no longer said.
            let away = p.away.as_ref().map(|a| match hid {
                true => "",
                false => a.headline.as_str(),
            });
            let (purpose, state) = brief::floor_line(&shown, away);
            let state = workforce_common::redact::redact_secrets(&state);
            crate::floor::agents::briefed(
                log.as_ref(),
                service.agent_id.as_str(),
                owner,
                session_id,
                &purpose,
                &state,
            );
        }
        #[cfg(not(feature = "postgres"))]
        let _ = (service, owner, session_id, p);
    }

    /// The suggestions quiet check-ins made since `p`'s last update, shown
    /// with it: no message of their own carries them.
    fn quiet_suggestions(
        &self,
        boss: &str,
        p: &Pending,
    ) -> Vec<workforce_desk::crew::MentionedSuggestion> {
        let cutoff = p.cutoff.unwrap_or_else(Utc::now);
        self.quiet_made
            .lock()
            .get(boss)
            .map(|made| {
                made.iter()
                    .filter(|(at, _)| *at <= cutoff)
                    .map(|(_, s)| s.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Keeps the suggestions `boss`'s quiet check-in made since `since`
    /// for its next update.
    pub(super) async fn keep_quiet_suggestions(&self, boss: &str, since: DateTime<Utc>) {
        let made = match self.desk.store().suggestions_since(boss, since).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "reading a quiet check-in's suggestions failed");
                return;
            }
        };
        if made.is_empty() {
            return;
        }
        let now = Utc::now();
        let mut kept = self.quiet_made.lock();
        let list = kept.entry(boss.to_string()).or_default();
        for s in made {
            if !list.iter().any(|(_, k)| k.id == s.id) {
                list.push((
                    now,
                    workforce_desk::crew::MentionedSuggestion {
                        id: s.id,
                        text: s.text,
                        state: s.state,
                    },
                ));
            }
        }
    }

    /// The person was told `p`: its restated items counted, the
    /// suggestions it carried let go, and the state moved on as of what it
    /// was written from.
    async fn told(
        &self,
        service: &AgentService,
        owner: &str,
        state: &UpdateState,
        p: &Pending,
    ) -> eyre::Result<()> {
        let store = self.desk.store();
        let now = Utc::now();
        let cutoff = p.cutoff.unwrap_or(now);
        // A catch-up ends the absence it covered, once; it, or a roundup,
        // says all that happened up to it, so the next roundup starts there.
        if let Some(a) = &p.away {
            store
                .update_back(service.agent_id.as_str(), a.since)
                .await?;
        }
        if p.away.is_some() || p.update.roundup.is_some() {
            store
                .update_rounded(service.agent_id.as_str(), cutoff)
                .await?;
        }
        for (item, park) in &p.restated {
            if let Err(e) = store.item_said(*item, now, *park).await {
                tracing::warn!(error = %e, "counting a restated item failed");
            }
        }
        if let Some(made) = self.quiet_made.lock().get_mut(service.agent_id.as_str()) {
            made.retain(|(at, _)| *at > cutoff);
        }
        self.closing_told(service.agent_id.as_str(), cutoff);
        let read = service
            .sessions
            .desk_read(owner, DESK_CHANNEL)
            .ok()
            .flatten()
            .map(|r| r.at);
        // The last one unread as this one goes: one more in a row.
        let unread = update::streak(state.unread, state.last_update_at, read) + 1;
        store
            .update_sent(service.agent_id.as_str(), cutoff, &p.told, unread)
            .await
    }
}

/// What a check-in's prompt names a handed update by.
fn marker(p: &Pending) -> String {
    format!("update {}", p.id)
}

/// A boss's update being looked at, by one look at a time; let go when
/// dropped.
pub(super) struct Updating<'a>(
    &'a parking_lot::Mutex<std::collections::HashSet<String>>,
    String,
);

impl<'a> Updating<'a> {
    fn begin(
        set: &'a parking_lot::Mutex<std::collections::HashSet<String>>,
        boss: &str,
    ) -> Option<Self> {
        set.lock()
            .insert(boss.to_string())
            .then(|| Self(set, boss.to_string()))
    }
}

impl Drop for Updating<'_> {
    fn drop(&mut self) {
        self.0.lock().remove(&self.1);
    }
}

/// Looks at the bosses' updates every minute: an update gathers for
/// minutes, so a finer look buys nothing.
pub(super) async fn updates_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let mut every = tokio::time::interval(crew.timing.minute);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = every.tick() => {
                // Held needs-you a restart left: theirs by now.
                crew.post_due_cards().await;
                // A boss newly at Act fully asks the person's directives.
                crew.look_at_directives().await;
                crew.look_at_updates().await;
            }
            // The person paused typing, came back, or wrote: look now.
            () = crew.update_poke.notified() => crew.look_at_updates().await,
        }
    }
}
