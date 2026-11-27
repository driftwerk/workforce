//! A person's request their agent elsewhere handed to their Desk boss
//! (DW-770, designs/dw-770-remote-goal-to-desk.md): kept once, delivered to
//! one boss as the person's own words, and Desk's word on it owed back to
//! where they asked ([`workforce_desk::handed`]).
//!
//! - **Only the person's.** A request comes only from a link the person
//!   whose Desk this is made; any other, or one Desk can't tell is theirs,
//!   is refused and never delivered.
//! - **To one boss.** The boss of the session it names (this computer's or
//!   a machine's), else the main one. With none, the person is told what
//!   turns one on.
//! - **Their words, bounded.** One relayed turn of the boss's DM whose
//!   message is their words, never read as the person's own typing: no
//!   consent to anything else, no reply to what the boss proposed, no order
//!   to another agent. They agree one thing alone: a new plan's goal that
//!   they say ([`CrewHub::handed_agreed`]).
//! - **Said back in Desk's own words.** Whether the boss keeps it as a goal,
//!   and each stall of that goal after (`CrewHub::flag_handed`); never the
//!   boss's answer or an error.
//! - **Taken again** when the Desk that took it stopped first: delivered
//!   if it never was, and said to be in the boss's DM if its turn's answer
//!   was lost.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, SubsecRound, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use eyre::Result;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Scope;
use workforce_desk::crew::one_line;
use workforce_desk::handed::{agrees, covers, Handed, Kept, WORDS_MAX};
use workforce_desk::plan::PlanState;
use workforce_desk::Speaker;

use super::{name_of, AgentService, CrewHub};

const NOT_YOURS: &str = "This Desk isn't yours: the person whose Desk it is linked me, so it took nothing. Ask me from your own Desk's link.";
const OWNER_UNKNOWN: &str = "Your Desk can't tell yet whose it is, so it took nothing: open Desk on your computer and say hello to your boss there once. Then ask me again.";
const NO_BOSS: &str = "You have no boss in Desk yet: open an agent's settings in Desk and turn on Oversees the crew. Then ask me again.";
const TOO_LONG: &str =
    "Your request is too long for your Desk to take: ask me again in a shorter message.";
const SESSION_GONE: &str = "Desk doesn't have the session you named any more, so nothing was handed on. Ask me again about the work as it is now.";
const SESSION_UNREADABLE: &str = "Your Desk couldn't tell which session your request is about, so nothing was handed on. Ask me again, naming the session as Desk shows it.";

/// The requests whose boss's turn runs in this process now, by agent and
/// id, as delivered.
static DELIVERING: LazyLock<Mutex<HashMap<(String, Uuid), Handed>>> =
    LazyLock::new(Default::default);

/// One request's boss's turn under way, for as long as it is held.
struct Delivering((String, Uuid));

impl Delivering {
    fn begin(h: &Handed) -> Self {
        let key = (h.agent.clone(), h.id);
        DELIVERING.lock().insert(key.clone(), h.clone());
        Self(key)
    }
}

impl Drop for Delivering {
    fn drop(&mut self) {
        DELIVERING.lock().remove(&self.0);
    }
}

/// The ids of the requests taken through `agent` whose boss's turn runs
/// now: its hub keeps them this Desk's.
pub(crate) fn delivering(agent: &str) -> Vec<Uuid> {
    DELIVERING
        .lock()
        .keys()
        .filter(|(a, _)| a == agent)
        .map(|(_, id)| *id)
        .collect()
}

/// What the person is told of a request boss `boss` holds whose answer
/// was lost: Desk stopped while its turn ran.
fn unanswered(boss: &str) -> String {
    format!("Your Desk boss {boss} has your request in your Desk DM, but Desk stopped before it answered: answer there, or ask me again.")
}

/// What the person is told when no boss oversees session `name`'s group.
fn no_group_boss(name: &str) -> String {
    format!("No boss oversees “{name}”'s group in Desk: open a group's boss settings and turn on Oversees the crew. Then ask me again.")
}

fn has_it(boss: &str, agreed: &str) -> String {
    format!(
        "Your Desk boss {boss} has it: it keeps “{}” as a goal, and Desk tells you here when it stalls.",
        one_line(agreed, 200)
    )
}

fn not_a_goal_yet(boss: &str) -> String {
    format!("Your Desk boss {boss} has your request in your Desk DM. It isn't kept as a goal yet, so Desk can't flag stalls on it until it is: answer {boss} there if it asks you anything.")
}

fn could_not_take(boss: &str) -> String {
    format!("Your Desk boss {boss} couldn't take your request just now. Ask me again in a while.")
}

/// What boss `boss`'s turn on `h` is told beside the person's words, for
/// `person` and the session `session` names.
fn told(h: &Handed, person: &str, session: Option<&str>) -> String {
    let on = session.map_or_else(String::new, |s| format!(", on the session “{s}”"));
    format!(
        "This is {person}'s own request, in their words, passed on by {agent}, their agent elsewhere, from their {place} conversation with it. They want this kept on track and any stall flagged: keep it as a goal with crew.plan, its goal in their own words and agreed: their words, exactly as here{on}. Their words agree that goal alone, nothing else. Desk tells {agent} of each stall, and {agent} tells them in {place}.",
        agent = h.agent_name,
        place = h.place,
    )
}

/// A boss and how it oversees.
type Boss = (Arc<AgentService>, workforce_common::oversight::Oversight);

/// Where the session a request names is, as Desk names it.
enum Place {
    Here(Uuid),
    Machine(String, Uuid),
}

fn place_of(session: &str) -> Option<Place> {
    match session.parse() {
        Ok(id) => Some(Place::Here(id)),
        Err(_) => {
            super::relay::machine_session(session).map(|(m, id)| Place::Machine(m.into(), id))
        }
    }
}

impl CrewHub {
    /// Takes request `h` from the link `person` made: kept once, then
    /// delivered on its own task, so a long turn never holds up the share.
    pub(crate) async fn take_handed(self: &Arc<Self>, person: &str, h: Handed) -> Result<()> {
        if DELIVERING.lock().contains_key(&(h.agent.clone(), h.id)) {
            return Ok(());
        }
        let store = self.desk.store();
        match store.handed_keep(&h).await? {
            Kept::New | Kept::Undelivered => {}
            Kept::Done => return Ok(()),
            Kept::Unanswered { boss } => {
                return store
                    .handed_say(&h, &unanswered(&self.boss_name_of(&boss)))
                    .await
            }
        }
        let refused = match self.person(None) {
            Some(p) if p == person => None,
            Some(_) => Some(NOT_YOURS.to_string()),
            None => Some(OWNER_UNKNOWN.to_string()),
        };
        let refused =
            refused.or_else(|| (h.words.chars().count() > WORDS_MAX).then(|| TOO_LONG.to_string()));
        if let Some(why) = refused {
            return store.handed_say(&h, &why).await;
        }
        let (boss, name) = match self.handed_boss(h.session.as_deref()) {
            Ok(found) => found,
            Err(why) => return store.handed_say(&h, &why).await,
        };
        let service = boss.0;
        let h = Handed {
            boss: Some(service.agent_id.to_string()),
            delivered_at: Some(Utc::now().trunc_subsecs(6)),
            ..h
        };
        let crew = Arc::clone(self);
        let owner = person.to_string();
        let settling = crate::shutdown::Settling::begin();
        let delivering = Delivering::begin(&h);
        tokio::spawn(async move {
            let _settling = settling;
            let _delivering = delivering;
            crew.deliver_handed(&service, &owner, &h, name.as_deref())
                .await;
        });
        Ok(())
    }

    /// The boss a request about `session` goes to, with the session's
    /// name; or what the person is told when it can go to none.
    fn handed_boss(&self, session: Option<&str>) -> Result<(Boss, Option<String>), String> {
        let Some(session) = session else {
            return self
                .overseer_for(&Scope::Ungrouped)
                .map(|b| (b, None))
                .ok_or_else(|| NO_BOSS.to_string());
        };
        match place_of(session) {
            None => Err(SESSION_UNREADABLE.into()),
            Some(Place::Here(id)) => {
                if self.desk.view(id).is_err() {
                    return Err(SESSION_GONE.into());
                }
                let name = self.name_of_session(id);
                self.overseer_of_session(id)
                    .map(|b| (b, Some(name.clone())))
                    .ok_or_else(|| no_group_boss(&name))
            }
            Some(Place::Machine(machine, id)) => {
                let name = format!("{machine}'s session {}", &id.to_string()[..8]);
                let group = self
                    .desk
                    .group_of_member(&format!("session:{machine}/{id}"));
                self.overseer_of_group(group.as_deref())
                    .map(|b| (b, Some(name.clone())))
                    .ok_or_else(|| no_group_boss(&name))
            }
        }
    }

    /// One turn of `service`'s DM with `owner` on `h`, their words relayed,
    /// kept as delivered once they are in the DM, and the word back it
    /// earns.
    async fn deliver_handed(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        h: &Handed,
        session: Option<&str>,
    ) {
        let person = crate::desk_surface::person_name(self.state.identity_store.as_ref(), owner);
        let by = match &person {
            Some(p) => format!("{}, for {p} in {}", h.agent_name, h.place),
            None => format!("{}, in {}", h.agent_name, h.place),
        };
        let author = Speaker::overseer(&h.agent, &by);
        let told = told(h, person.as_deref().unwrap_or("the person"), session);
        let boss = service.agent_id.as_str();
        let store = self.desk.store();
        let at = h.delivered_at.unwrap_or_else(Utc::now);
        let (kept, in_dm) = tokio::sync::oneshot::channel();
        let ran = self.run_dm_turn_queued(service, owner, || {
            let (author, told) = (author.clone(), told.as_str());
            async move {
                let turn = self.run_turn_with(
                    service,
                    owner,
                    DESK_CHANNEL,
                    &h.words,
                    1,
                    None,
                    Some(author),
                    None,
                    Some(told),
                );
                self.with_mind(boss, "a request handed from elsewhere", turn)
                    .await
            }
        });
        let noted = async {
            let saved = in_dm.await.is_ok();
            if saved {
                if let Err(e) = store.handed_delivered(h, boss, at).await {
                    tracing::warn!(handed = %h.id, error = %format!("{e:#}"), "where a handed request went wasn't kept");
                }
            }
            saved
        };
        let (ran, saved) = tokio::join!(super::deliver::told_when_asked(kept, ran), noted);
        let name = name_of(service);
        let said = match (ran, saved) {
            (Ok(_), _) => match self.covering_goal(h).await {
                Some(agreed) => has_it(&name, &agreed),
                None => not_a_goal_yet(&name),
            },
            (Err(e), true) => {
                tracing::warn!(handed = %h.id, boss = %boss, error = %format!("{e:#}"), "a handed request's turn failed");
                not_a_goal_yet(&name)
            }
            (Err(e), false) => {
                tracing::warn!(handed = %h.id, boss = %boss, error = %format!("{e:#}"), "a handed request's turn never began");
                could_not_take(&name)
            }
        };
        if let Err(e) = store.handed_say(h, &said).await {
            tracing::warn!(handed = %h.id, error = %format!("{e:#}"), "a handed request's word back wasn't kept");
        }
    }

    /// The words a plan of `h`'s boss that carries `h` was agreed in, if
    /// one does.
    async fn covering_goal(&self, h: &Handed) -> Option<String> {
        let boss = h.boss.as_deref()?;
        let plans = self.desk.store().plans(None).await.ok()?;
        for plan in plans
            .into_iter()
            .filter(|p| p.boss == boss && matches!(p.state, PlanState::Running | PlanState::Paused))
        {
            let Ok(detail) = self.desk.plan(plan.id).await else {
                continue;
            };
            let sessions =
                super::goal_check::session_ids(&super::goal_check::plan_sessions(&detail));
            let plan = &detail.summary.plan;
            if covers(h, plan, &sessions) {
                return Some(plan.agreed.clone().unwrap_or_else(|| plan.goal.clone()));
            }
        }
        None
    }

    /// When the person agreed `goal` in `quote` for boss `service`: in their
    /// DM ([`Self::agreed_at`]), or in a request of theirs delivered to it
    /// ([`Self::handed_agreed`]).
    pub(super) async fn agreed_or_handed(
        &self,
        service: &AgentService,
        quote: &str,
        goal: &str,
    ) -> Result<DateTime<Utc>, String> {
        match self.agreed_at(service, quote, goal) {
            Ok(at) => Ok(at),
            Err(why) => self
                .handed_agreed(service.agent_id.as_str(), quote, goal)
                .await
                .ok_or(why),
        }
    }

    /// When a request handed to boss `boss` within the last day agreed a
    /// new plan's goal `goal` in `quote`: its words hold `quote`, say `goal`
    /// and agree it, and no plan carries it yet. Then, when it reached the
    /// boss.
    pub(super) async fn handed_agreed(
        &self,
        boss: &str,
        quote: &str,
        goal: &str,
    ) -> Option<DateTime<Utc>> {
        let since = Utc::now() - super::goal::AGREED_WITHIN;
        let running: Vec<Handed> = DELIVERING
            .lock()
            .values()
            .filter(|h| h.boss.as_deref() == Some(boss))
            .cloned()
            .collect();
        let kept = self
            .desk
            .store()
            .handed_of_boss(boss)
            .await
            .unwrap_or_default();
        for h in running.into_iter().chain(kept) {
            let fits = h.delivered_at.is_some_and(|at| at > since)
                && workforce_desk::goal::says_goal(&h.words, quote)
                && agrees(&h, goal);
            if fits && self.covering_goal(&h).await.is_none() {
                return h.delivered_at;
            }
        }
        None
    }

    /// Says `line`, boss `service`'s word on its goal plan `plan` stalling,
    /// back where each request it carries was asked.
    pub(super) async fn flag_handed(
        &self,
        service: &AgentService,
        plan: &workforce_desk::plan::Plan,
        sessions: &[String],
        line: &str,
    ) {
        let store = self.desk.store();
        let handed = match store.handed_of_boss(&plan.boss).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(plan = %plan.id, error = %format!("{e:#}"), "the requests a goal carries weren't read");
                return;
            }
        };
        let said = format!("{} (your Desk boss): {line}", name_of(service));
        for h in handed.iter().filter(|h| covers(h, plan, sessions)) {
            if let Err(e) = store.handed_say(h, &said).await {
                tracing::warn!(handed = %h.id, error = %format!("{e:#}"), "a stall's word back wasn't kept");
            }
        }
    }
}
