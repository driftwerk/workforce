//! A person's request their agent elsewhere handed to their Desk boss
//! (DW-770, designs/dw-770-remote-goal-to-desk.md): kept once, delivered to
//! one boss as the person's own words, and Desk's word on it owed back to
//! where they asked ([`workforce_desk::handed`]).
//!
//! - **Only the person's.** A request comes only from a link the person
//!   whose Desk this is made; any other is refused and never delivered.
//! - **To one boss.** The boss of the session it names, else the main one.
//!   With none, the person is told what turns one on.
//! - **As their own words.** One turn of the boss's DM whose message is
//!   their words, exactly, relayed by the agent that took them: every check
//!   of the person's words reads it as if they typed it there.
//! - **Said back.** Whether the boss keeps it as a goal, and each stall of
//!   that goal after (`CrewHub::flag_handed`).
//! - **Taken again** when the Desk that took it stopped first: delivered
//!   if it never was, and said to be in the boss's DM if its turn's answer
//!   was lost.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use eyre::Result;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Scope;
use workforce_desk::crew::one_line;
use workforce_desk::handed::{covers, Handed, Kept};
use workforce_desk::plan::PlanState;
use workforce_desk::Speaker;

use super::{name_of, AgentService, CrewHub};

const NOT_YOURS: &str = "This Desk isn't yours: the person whose Desk it is linked me, so it took nothing. Ask me from your own Desk's link.";
const NO_BOSS: &str = "You have no boss in Desk yet: open an agent's settings in Desk and turn on Oversees the crew. Then ask me again.";

/// The requests whose boss's turn runs in this process now.
static DELIVERING: LazyLock<Mutex<HashSet<uuid::Uuid>>> = LazyLock::new(Default::default);

/// One request's boss's turn under way, for as long as it is held.
struct Delivering(uuid::Uuid);

impl Delivering {
    fn begin(id: uuid::Uuid) -> Self {
        DELIVERING.lock().insert(id);
        Self(id)
    }
}

impl Drop for Delivering {
    fn drop(&mut self) {
        DELIVERING.lock().remove(&self.0);
    }
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

/// What the person is told when Desk no longer has session `name`.
fn session_gone(name: &str) -> String {
    format!("Desk doesn't have the session “{name}” any more, so nothing was handed on. Ask me again about the work as it is now.")
}

/// What boss `boss`'s turn on `h` is told beside the person's words, for
/// `person` and the session `session` names.
fn told(h: &Handed, person: &str, session: Option<&str>) -> String {
    let on = session.map_or_else(String::new, |s| format!(", on the session “{s}”"));
    format!(
        "This is {person}'s own request, in their words, passed on by {agent}, their agent elsewhere, from their {place} conversation with it. They want this kept on track and any stall flagged: keep it as a goal with crew.plan, its goal in their own words and agreed: their words, exactly as here{on}. Desk tells {agent} of each stall, and {agent} tells them in {place}.",
        agent = h.agent_name,
        place = h.place,
    )
}

impl CrewHub {
    /// Takes request `h` from the link `person` made: kept once, then
    /// delivered on its own task, so a long turn never holds up the share.
    pub(crate) async fn take_handed(self: &Arc<Self>, person: &str, h: Handed) -> Result<()> {
        let store = self.desk.store();
        match store.handed_keep(&h).await? {
            Kept::New | Kept::Undelivered => {}
            Kept::Done => return Ok(()),
            Kept::Unanswered { .. } if DELIVERING.lock().contains(&h.id) => return Ok(()),
            Kept::Unanswered { boss } => {
                return store
                    .handed_say(h.id, &unanswered(&self.boss_name_of(&boss)))
                    .await
            }
        }
        if self.person(None).is_some_and(|p| p != person) {
            return store.handed_say(h.id, NOT_YOURS).await;
        }
        let session = h.session.map(|s| (s, self.name_of_session(s)));
        let boss = match &session {
            Some((s, name)) if self.desk.view(*s).is_err() => Err(session_gone(name)),
            Some((s, name)) => self
                .overseer_of_session(*s)
                .ok_or_else(|| no_group_boss(name)),
            None => self
                .overseer_for(&Scope::Ungrouped)
                .ok_or_else(|| NO_BOSS.to_string()),
        };
        let service = match boss {
            Ok((service, _)) => service,
            Err(why) => return store.handed_say(h.id, &why).await,
        };
        let delivered = store
            .handed_delivered(h.id, service.agent_id.as_str())
            .await?;
        let h = Handed {
            boss: Some(service.agent_id.to_string()),
            delivered_at: Some(delivered),
            ..h
        };
        let crew = Arc::clone(self);
        let owner = person.to_string();
        let name = session.map(|(_, name)| name);
        let settling = crate::shutdown::Settling::begin();
        let delivering = Delivering::begin(h.id);
        tokio::spawn(async move {
            let _settling = settling;
            let _delivering = delivering;
            crew.deliver_handed(&service, &owner, &h, name.as_deref())
                .await;
        });
        Ok(())
    }

    /// One turn of `service`'s DM with `owner` on `h`, their words as
    /// theirs, and the word back it earns.
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
        let ran = self
            .run_dm_turn_queued(service, owner, || {
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
                        Some(h.words.clone()),
                        Some(told),
                    );
                    self.with_mind(boss, "a request handed from elsewhere", turn)
                        .await
                }
            })
            .await;
        let name = name_of(service);
        let said = match ran {
            Ok(ran) => match self.covering_goal(h).await {
                Some(goal) => format!(
                    "Your Desk boss {name} has it: it keeps “{}” as a goal, and Desk tells you here when it stalls.",
                    one_line(&goal, 200)
                ),
                None => format!(
                    "Your Desk boss {name} has your request and answered: “{}”. It isn't kept as a goal yet, so Desk can't flag stalls on it until it is: if it waits on your OK, give it in your Desk DM with {name}.",
                    one_line(&ran.answer, 600)
                ),
            },
            Err(e) => {
                tracing::warn!(handed = %h.id, boss = %boss, error = %format!("{e:#}"), "a handed request's turn failed");
                format!(
                    "Your Desk boss {name} couldn't take your request just now ({}). It is in your Desk DM with {name}: answer there, or ask me again.",
                    one_line(&format!("{e}"), 200)
                )
            }
        };
        if let Err(e) = self.desk.store().handed_say(h.id, &said).await {
            tracing::warn!(handed = %h.id, error = %format!("{e:#}"), "a handed request's word back wasn't kept");
        }
    }

    /// The goal of a plan of `h`'s boss that carries `h`, if one does.
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
            let sessions = super::goal_check::session_ids(&detail);
            if covers(h, &detail.summary.plan, &sessions) {
                return Some(detail.summary.plan.goal);
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
            if let Err(e) = store.handed_say(h.id, &said).await {
                tracing::warn!(handed = %h.id, error = %format!("{e:#}"), "a stall's word back wasn't kept");
            }
        }
    }
}
