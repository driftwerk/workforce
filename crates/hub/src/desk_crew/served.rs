//! The deciding side of a boss's crew call (remote bosses, RB2): a call
//! from a boss served from a grant (a boss on another hub, or the home's
//! own boss of a machine's sessions) is decided here, on the engine that
//! holds the session, by [`CrewHub::serve_crew`], whatever carried it. The
//! carriers come later (the relay, RB3; the uplink, RB5): nothing reaches
//! this yet but its tests.
//!
//! Every call goes through the same steps, in order, each refusal in plain
//! words (design §4.1):
//!
//! 1. it is recorded before anything is done, so one sent again is never
//!    acted on twice;
//! 2. the grant is live (not paused, stopped, expired or unreadable);
//! 3. the caller is the grant's boss, exactly;
//! 4. the tool is one served, and what it names is this engine's session in
//!    the grant (`members` or `born`), or it is "not ours";
//! 5. the never-list, at every level short of unbound;
//! 6. the level: the grant's, never the caller's word, through the same
//!    gate a local overseer's calls go through, the boss named, unbound
//!    by the same check ([`CrewHub::unbound_given`]): Act fully, anywhere,
//!    every switch on and a daily budget, as the grant says now;
//! 7. what it may see ([`workforce_desk::sees`]), every string redacted
//!    whole and capped.
//!
//! The boss here is a value the grant gives, `<hub>/<agent>`, never looked
//! up by its scope or as one of this hub's agents, so a local overseer of
//! the same group is never taken for it, nor it for one.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Act, Full, Level, Oversight, Scope, TakeOn};
use workforce_desk::charge::Wheel;
use workforce_desk::crew::{By, ItemKind, Party};
use workforce_desk::grant::{self, BossRef, Grant};
use workforce_desk::grant_store::{Begun, CallRow};
use workforce_desk::model::{Engine, NewSession};
use workforce_desk::sees::{self, Part};

use super::actions::{Action, Gate, Target};
use super::watcher::reported;
use super::CrewHub;

/// The reading tools served.
pub const SERVED_READ: [&str; 3] = ["crew.overview", "crew.activity", "crew.history"];
/// The acting tools served.
pub const SERVED_ACT: [&str; 10] = [
    "crew.nudge",
    "crew.escalate",
    "crew.approve",
    "crew.deny",
    "crew.stop",
    "crew.interrupt",
    "crew.resume",
    "crew.restart",
    "crew.start_session",
    "crew.release",
];

/// The most sessions a served boss runs at once when its grant doesn't
/// say.
const PARALLEL: u32 = 2;

/// One crew call as its carrier brings it.
#[derive(Debug, Clone)]
pub struct ServedCall {
    pub tool: String,
    pub input: Value,
    /// Who asks: what the carrier proved (the link's boss).
    pub boss: BossRef,
    /// The boss link it came on, and the boss hub's id for the call: one
    /// call is acted on once.
    pub link_id: Option<String>,
    pub call_id: Option<String>,
    /// What the boss may do where it runs, when its carrier says (a local
    /// overseer's own level and switches, relayed): the call is decided at
    /// the lower of it and the grant's, never above either.
    pub ceiling: Option<Ceiling>,
}

/// Whether switch `k` of `act` is on: off unless said so.
fn switch_on(act: &Value, k: &str) -> bool {
    act.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// The switches `act` turns on, each off unless it says so.
pub fn act_of(act: &Value) -> Act {
    let on = |k: &str| switch_on(act, k);
    Act {
        nudge: on("nudge"),
        resume_restart: on("resume_restart"),
        read_only_approvals: on("read_only_approvals"),
        hand_out_work: on("hand_out_work"),
        interrupt: on("interrupt"),
        message_agents: on("message_agents"),
        message_terminal_sessions: on("message_terminal_sessions"),
        answer_sessions: on("answer_sessions"),
        hand_off: on("hand_off"),
        set_mode: on("set_mode"),
    }
}

/// A boss's own level and switches, as its home holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ceiling {
    pub level: Level,
    pub act: Act,
    /// `[oversight.full] anywhere` at its home: off when not said, so a
    /// home that doesn't say it never makes it unbound here.
    pub anywhere: bool,
}

impl Ceiling {
    /// What a relayed call says of its boss: its level, and `act` with
    /// its switches and `anywhere`, each off unless said so.
    pub fn of(level: Level, act: &Value) -> Self {
        Self {
            level,
            act: act_of(act),
            anywhere: switch_on(act, "anywhere"),
        }
    }

    /// As a relayed call carries it: the switches and `anywhere` in one
    /// object, which a machine that doesn't know `anywhere` reads past.
    pub fn act_value(&self) -> Value {
        let mut v = serde_json::to_value(self.act).unwrap_or_else(|_| json!({}));
        v["anywhere"] = json!(self.anywhere);
        v
    }
}

fn rank(l: Level) -> u8 {
    match l {
        Level::Watch => 0,
        Level::Suggest => 1,
        Level::Act => 2,
        Level::Full => 3,
    }
}

/// `o` no higher than `c`: the lower level, and each switch on only where
/// both have it on.
pub fn narrowed(mut o: Oversight, c: &Ceiling) -> Oversight {
    if rank(c.level) < rank(o.level) {
        o.level = c.level;
    }
    let (a, b) = (&mut o.act, &c.act);
    a.nudge &= b.nudge;
    a.resume_restart &= b.resume_restart;
    a.read_only_approvals &= b.read_only_approvals;
    a.hand_out_work &= b.hand_out_work;
    a.interrupt &= b.interrupt;
    a.answer_sessions &= b.answer_sessions;
    a.message_agents &= b.message_agents;
    a.message_terminal_sessions &= b.message_terminal_sessions;
    a.hand_off &= b.hand_off;
    a.set_mode &= b.set_mode;
    o.full.anywhere &= c.anywhere;
    o
}

/// `o` for a served boss short of unbound: what this hub's person keeps,
/// whatever the grant says (messaging agents and terminals' sessions,
/// continuing in another engine, a session's mode), off. Unbound, it is
/// held only as a local unbound boss is.
pub fn held_below_unbound(mut o: Oversight) -> Oversight {
    o.act.message_agents = false;
    o.act.message_terminal_sessions = false;
    o.act.hand_off = false;
    o.act.set_mode = false;
    o
}

/// The daily budget `act` gives, in cents: an amount in dollars above 0
/// and up to Act fully's most; none for anything else.
fn budget_of(act: &Value) -> Option<u64> {
    let usd = act.get("budget_usd_per_day")?.as_f64()?;
    (usd.is_finite() && usd > 0.0 && usd <= workforce_common::oversight::MAX_FULL_BUDGET_USD)
        .then(|| ((usd * 100.0).round() as u64).max(1))
}

/// What became of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServedStatus {
    Ok,
    /// Not done: it is the person's to decide.
    Suggested,
    Refused,
}

impl ServedStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Suggested => "suggested",
            Self::Refused => "refused",
        }
    }
}

/// The answer to a served call: already filtered, redacted and capped.
#[derive(Debug, Clone, Serialize)]
pub struct Served {
    pub status: ServedStatus,
    pub body: Value,
    /// Its row in `boss_calls`.
    pub audit: Option<Uuid>,
}

fn refused(why: impl Into<String>) -> (ServedStatus, Value) {
    (ServedStatus::Refused, json!({ "error": why.into() }))
}

/// The oversight a grant gives its boss, and nothing else: its level, its
/// act switches, `anywhere` and daily budget (`budget_usd_per_day`), each
/// off or none unless the grant says so, sessions only as handed or
/// started, and Act fully's repositories. Short of a budget, Act fully
/// isn't in force ([`super::fully_for`]). There is no field that widens it
/// beyond the grant's group: never Ungrouped for a boss on another hub.
/// What this hub's person keeps below unbound is taken off as each call is
/// decided ([`held_below_unbound`]).
pub fn served_oversight(g: &Grant) -> Oversight {
    let parallel = g
        .act
        .get("parallel")
        .and_then(Value::as_u64)
        .map_or(PARALLEL, |p| {
            p.clamp(1, workforce_common::oversight::MAX_PARALLEL as u64) as u32
        });
    let mut o = Oversight {
        enabled: true,
        scope: match g.group.as_str() {
            workforce_desk::UNGROUPED => Scope::Ungrouped,
            group => Scope::Group(group.to_string()),
        },
        level: match g.level {
            grant::Level::Watch => Level::Watch,
            grant::Level::Suggest => Level::Suggest,
            grant::Level::Act => Level::Act,
            grant::Level::Full => Level::Full,
        },
        act: act_of(&g.act),
        full: Full {
            repos: g.repos.clone(),
            budget_cents_per_day: budget_of(&g.act),
            workflows: Vec::new(),
            anywhere: switch_on(&g.act, "anywhere"),
            budget_cleared_at: None,
        },
        ..Oversight::default()
    };
    o.work.take_on = TakeOn::Handed;
    o.work.parallel = parallel;
    o.work.engine = Engine::Claude.as_str().to_string();
    o
}

/// Whether `folder`, said plainly, is inside one of `allowed` on this
/// computer: each read where it really leads, compared part by part, so
/// `/src/acme-evil` is not inside `/src/acme` and a link doesn't lead out.
pub fn folder_inside(folder: &str, allowed: &[String]) -> Option<std::path::PathBuf> {
    if !grant::canonical(folder) {
        return None;
    }
    let real = std::fs::canonicalize(folder).ok()?;
    allowed
        .iter()
        .any(|a| grant::canonical(a) && std::fs::canonicalize(a).is_ok_and(|a| real.starts_with(a)))
        .then_some(real)
}

/// The group rows of `g`'s sessions are stamped with: none for Ungrouped.
fn stamped_as(g: &Grant) -> Option<&str> {
    (g.group != workforce_desk::UNGROUPED).then_some(g.group.as_str())
}

/// Every string of `v`, secrets hidden whole and each capped: how many
/// were changed.
fn cleaned(v: &mut Value) -> usize {
    match v {
        Value::String(s) => {
            let hidden = crate::redact::hide_secrets(s);
            let capped = sees::capped(&hidden, sees::MAX_CHARS);
            let changed = capped != *s;
            *s = capped;
            usize::from(changed)
        }
        Value::Array(a) => a.iter_mut().map(cleaned).sum(),
        Value::Object(o) => o.values_mut().map(cleaned).sum(),
        _ => 0,
    }
}

impl CrewHub {
    /// Decides `call` for `group`'s grant on this engine, as the module
    /// says, and records what came of it.
    pub async fn serve_crew(self: &Arc<Self>, call: ServedCall, group: &str) -> Served {
        let tool = SERVED_READ
            .iter()
            .chain(SERVED_ACT.iter())
            .find(|t| **t == call.tool)
            .copied();
        let row = CallRow {
            caller: format!("boss:{}", call.boss.party_id()),
            boss: Some(call.boss.key()),
            group: Some(group.to_string()),
            tool: tool.unwrap_or("crew.unknown"),
            input: {
                let mut i = call.input.clone();
                cleaned(&mut i);
                sees::capped(&i.to_string(), 2000)
            },
            input_hash: {
                use sha2::Digest;
                hex::encode(sha2::Sha256::digest(call.input.to_string().as_bytes()))
            },
            outcome: "pending",
            said: None,
            result: json!({}),
        };
        // A boss on another hub's every call carries its link and its id,
        // or a repeat of it couldn't be told from a new one.
        if matches!(call.boss, BossRef::Remote { .. })
            && (call.link_id.as_deref().is_none_or(str::is_empty)
                || call.call_id.as_deref().is_none_or(str::is_empty))
        {
            return Served {
                status: ServedStatus::Refused,
                body: json!({ "error": "A call from another hub needs its link and its id." }),
                audit: None,
            };
        }
        let store = self.desk.store();
        let begun = store
            .boss_call_begin(call.link_id.as_deref(), call.call_id.as_deref(), &row)
            .await;
        let audit = match begun {
            Ok(Begun::New(id)) => id,
            Ok(Begun::Again { boss, .. }) if boss.as_deref() != Some(call.boss.key().as_str()) => {
                // Another boss's call id: nothing of it is said.
                return Served {
                    status: ServedStatus::Refused,
                    body: json!({ "error": workforce_desk::scope::not_ours(None) }),
                    audit: None,
                };
            }
            Ok(Begun::Again { outcome, said, .. }) => {
                // Sent again: never acted on twice; what came of it, said.
                return Served {
                    status: match outcome.as_str() {
                        "ok" => ServedStatus::Ok,
                        "suggested" => ServedStatus::Suggested,
                        _ => ServedStatus::Refused,
                    },
                    body: json!({ "again": true, "said": said }),
                    audit: None,
                };
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "a boss's call couldn't be recorded"
                );
                return Served {
                    status: ServedStatus::Refused,
                    body: json!({ "error": "This engine couldn't record the call, so it did nothing." }),
                    audit: None,
                };
            }
        };
        let (status, mut body) = match tool {
            None => refused(format!("{} isn't served to a boss here.", call.tool)),
            Some(tool) => self.decide_served(tool, &call, group).await,
        };
        let changed = cleaned(&mut body);
        let size = body.to_string().len();
        let said = body
            .get("done")
            .or_else(|| body.get("suggested"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .map(|s| sees::capped(s, 500));
        if let Err(e) = store
            .boss_call_end(
                audit,
                status.as_str(),
                said.as_deref(),
                &json!({ "bytes": size, "redacted": changed }),
            )
            .await
        {
            tracing::warn!(
                error = format!("{e:#}"),
                "what came of a boss's call wasn't recorded"
            );
        }
        Served {
            status,
            body,
            audit: Some(audit),
        }
    }

    async fn decide_served(
        self: &Arc<Self>,
        tool: &'static str,
        call: &ServedCall,
        group: &str,
    ) -> (ServedStatus, Value) {
        let grant = match self.desk.grant_live(group) {
            Ok(g) => g,
            Err(why) => return refused(why),
        };
        if !call.boss.servable() || grant.boss.as_ref() != Some(&call.boss) {
            return refused(workforce_desk::scope::not_ours(None));
        }
        let boss = call.boss.party_id();
        // What a boss relayed from the home says it may do there is what it
        // is judged by here until its next call says otherwise.
        if let Some(c) = &call.ceiling {
            self.ceilings.lock().insert(boss.clone(), c.clone());
        }
        let o = match &call.ceiling {
            Some(c) => narrowed(served_oversight(&grant), c),
            None => served_oversight(&grant),
        };
        // One boss per group: a local overseer of it means no grant's boss
        // is served here.
        if self.overseer_for(&o.scope).is_some() {
            return refused(format!(
                "{} already has a boss here, so no other is served for it.",
                grant.name
            ));
        }
        let result = if SERVED_READ.contains(&tool) {
            self.served_read(tool, &call.input, &grant, &boss)
                .await
                .map(|v| (ServedStatus::Ok, v))
        } else {
            // Whether it is unbound, judged now, from the grant as it is
            // and today's spend read now: never kept from an earlier call.
            self.read_served_spent(&boss).await;
            let unbound = self.served_full(&boss).is_some_and(|f| f.unbound);
            let o = match unbound {
                true => o,
                false => held_below_unbound(o),
            };
            self.served_act(tool, &call.input, &grant, &o, &boss, unbound)
                .await
        };
        // The boss answered a request itself: its ask to the person is
        // closed.
        if matches!(result, Ok((ServedStatus::Ok, _)))
            && matches!(tool, "crew.approve" | "crew.deny")
        {
            if let Some(approval) = call.input["approval_id"]
                .as_str()
                .and_then(|a| a.parse::<Uuid>().ok())
            {
                self.answered_by_boss(call.link_id.as_deref(), approval)
                    .await;
            }
        }
        result.unwrap_or_else(refused)
    }

    /// What `event` tells `boss`, the remote boss of `group` a link is for,
    /// as a wake-up (an uplink's `event` frame), or nothing: only while
    /// `group`'s grant is live and its boss is that very boss (as a call is
    /// served, [`Self::serve_crew`]); only a turn's end, a question, a failure
    /// or a limit of one of the grant's own sessions, had while it was in the
    /// grant's group; never what the boss itself or the person did, nor past
    /// the hand-off limit. Its words only when the grant's Sees shows them
    /// (`crew.activity`'s rule), and never a terminal.
    pub(crate) fn served_event(
        &self,
        group: &str,
        boss: &BossRef,
        event: &workforce_desk::crew::Event,
    ) -> Option<Value> {
        use workforce_desk::crew::{Kind, PartyKind};
        let g = self.desk.grant_live(group).ok()?;
        if !self.serves_remote(&g, boss) {
            return None;
        }
        if matches!(event.actor.kind, PartyKind::Manager | PartyKind::You)
            || event.hop >= i16::from(workforce_core::desk::MAX_HOPS)
        {
            return None;
        }
        // A request a session waits on reaches the boss by what is open
        // ([`Self::owed_remote`]), not by the event that raised it.
        if event.kind == Kind::NeedsYou && event.approval_id.is_some() {
            return None;
        }
        if !matches!(
            event.kind,
            Kind::Finished | Kind::Question | Kind::NeedsYou | Kind::Failed | Kind::Limit
        ) {
            return None;
        }
        let subject = event.subject.as_ref()?;
        let id: Uuid = subject.id.parse().ok()?;
        if !self.in_grant(&g, id) || event.group.as_deref() != stamped_as(&g) {
            return None;
        }
        let words = sees::shows(g.sees, Part::Words);
        let line = match words {
            true => format!(
                "{} {} ({}): {}{}",
                subject.name,
                event.kind.as_str(),
                subject.id,
                event.text,
                event
                    .reason
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default()
            ),
            false => format!("{} {} ({})", subject.name, event.kind.as_str(), subject.id),
        };
        Some(json!({
            "kind": event.kind.as_str(),
            "subject": subject.id,
            "line": sees::capped(&line, 2_000),
            "at": event.at,
            "hop": event.hop,
        }))
    }

    /// Marks `bosses` (each `<hub>/<agent>`) connected while the guard
    /// lives: an uplink carries their links.
    pub(crate) fn remote_up(self: &Arc<Self>, bosses: Vec<String>) -> RemoteUp {
        {
            let mut up = self.remote_up.lock();
            for b in &bosses {
                *up.entry(b.clone()).or_default() += 1;
            }
        }
        RemoteUp {
            crew: Arc::clone(self),
            bosses,
        }
    }

    /// Why each remote boss won't answer what the sessions it runs here
    /// ask, by its `<hub>/<agent>`, as the badge says: its grant doesn't
    /// serve it now, a local overseer of the group is its boss here, it
    /// only watches or suggests, Desk's breaker holds it, or no uplink
    /// carries its link. A boss left out answers. `local` are the scopes
    /// of this hub's own overseers now.
    pub(crate) fn remote_unanswering(&self, local: &[Scope]) -> Vec<(String, String)> {
        let mut why: Vec<(String, String)> = Vec::new();
        for (g, not_live) in self.desk.grants_bossed() {
            let Some(boss @ BossRef::Remote { .. }) = g.boss.as_ref() else {
                continue;
            };
            let party = boss.party_id();
            if why.iter().any(|(b, _)| *b == party) {
                continue;
            }
            let name = boss_name(&g);
            let o = served_oversight(&g);
            let reason = if let Some(not_live) = not_live {
                not_live
            } else if !boss.servable() || local.contains(&o.scope) {
                format!("{name} isn't {}'s boss here.", g.name)
            } else if o.level == Level::Watch {
                format!("{name} only watches.")
            } else if o.level == Level::Suggest {
                format!("{name} only suggests: you decide.")
            } else if let Some(held) = self.desk.agent_held_why(&party) {
                format!("{name} is {held}.")
            } else if self.remote_up.lock().get(&party).copied().unwrap_or(0) == 0 {
                format!("{name} isn't connected, so this is yours.")
            } else {
                continue;
            };
            why.push((party, reason));
        }
        why
    }

    /// Whether `boss` is the remote boss served for `g` here now: the
    /// grant's own boss, and no local overseer of the group (one boss per
    /// group, as a call is decided, [`Self::serve_crew`]).
    pub(super) fn serves_remote(&self, g: &Grant, boss: &BossRef) -> bool {
        matches!(boss, BossRef::Remote { .. })
            && boss.servable()
            && g.boss.as_ref() == Some(boss)
            && self.overseer_for(&served_oversight(g).scope).is_none()
    }

    /// The requests `boss`, the remote boss of `group`, owes an answer to
    /// and hasn't been given yet, as `event` frames: in each of the grant's
    /// sessions it runs and has the wheel of, at a level that does more
    /// than watch, the front request Desk can answer, while the boss isn't
    /// held by Desk's breaker. As a local boss's ([`Self::owed_asks`]):
    /// each is given once, `offered` keeping the ones given only while
    /// they are still open, so one raised while the person drove the
    /// session, one that became answerable later, and one open when the
    /// link connected all reach it. A session's words only where Sees
    /// shows its question. Each comes with the request it gives.
    pub(crate) fn owed_remote(
        &self,
        group: &str,
        boss: &BossRef,
        offered: &mut HashMap<(Uuid, String), Offered>,
    ) -> Vec<(Uuid, Value)> {
        let Ok(g) = self.desk.grant_live(group) else {
            offered.clear();
            return Vec::new();
        };
        if !self.serves_remote(&g, boss) || served_oversight(&g).level == Level::Watch {
            offered.clear();
            return Vec::new();
        }
        let party = boss.party_id();
        if self.desk.agent_held(&party).is_some() {
            return Vec::new();
        }
        let mut open = HashSet::new();
        let mut owed = Vec::new();
        for c in self.desk.charges() {
            if c.boss != party || !self.in_grant(&g, c.session) {
                continue;
            }
            let Ok(view) = self.desk.view(c.session) else {
                continue;
            };
            let Some(p) = view.pending.as_ref() else {
                continue;
            };
            open.insert(p.id);
            let given = (p.id, party.clone());
            if let Some(o) = offered.get_mut(&given) {
                // Given, and still open past [`LEFT_AFTER`] while the boss
                // has it: whatever became of it on the boss's hub, it is
                // the person's now, as a local boss's work turn leaves it.
                if !o.left && o.at.elapsed() >= LEFT_AFTER && c.wheel == Wheel::Boss {
                    o.left = true;
                    tracing::info!(session = %c.session, approval = %p.id, "a remote boss left a request open: it is the person's");
                    self.desk.leave_to_person(c.session, p.id);
                }
                continue;
            }
            if !p.answerable || p.request.is_none() || c.wheel != Wheel::Boss {
                continue;
            }
            offered.insert(
                given,
                Offered {
                    at: std::time::Instant::now(),
                    left: false,
                },
            );
            let line = if sees::shows(g.sees, Part::Question) {
                let what = match p.detail.trim() {
                    "" => p.tool.clone(),
                    detail => format!("{}: {}", p.tool, workforce_desk::crew::clip(detail, 300)),
                };
                format!(
                    "{} ({}) waits on a request: {what} (approval_id {})",
                    view.label(),
                    c.session,
                    p.id
                )
            } else {
                format!(
                    "{} ({}) waits on a request (approval_id {})",
                    view.label(),
                    c.session,
                    p.id
                )
            };
            owed.push((
                p.id,
                json!({
                    "kind": "needs_you",
                    "subject": c.session,
                    "line": sees::capped(&line, 2_000),
                    "at": chrono::Utc::now(),
                    "hop": 0,
                }),
            ));
        }
        offered.retain(|(id, _), _| open.contains(id));
        owed
    }

    /// Whether `id` is one of this engine's sessions in `g`.
    fn in_grant(&self, g: &Grant, id: Uuid) -> bool {
        (g.members.contains(&id) || g.born.contains(&id)) && self.desk.view(id).is_ok()
    }

    fn not_in_grant(&self, g: &Grant) -> String {
        workforce_desk::scope::not_ours(Some(&g.name))
    }

    async fn served_read(
        &self,
        tool: &str,
        input: &Value,
        g: &Grant,
        boss: &str,
    ) -> Result<Value, String> {
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %e, "a served crew reading failed");
            "This engine couldn't read its sessions just now.".to_string()
        };
        // Everything is read, then only the grant's own sessions kept: the
        // grant, not this engine's sidebar, says what is the boss's.
        let shows = |p| sees::shows(g.sees, p);
        match tool {
            "crew.overview" => {
                let o = self
                    .desk
                    .overview_for(self, None, None)
                    .await
                    .map_err(internal)?;
                let rows: Vec<Value> = o
                    .items
                    .iter()
                    .filter(|i| i.kind == ItemKind::Session)
                    .filter(|i| i.id.parse().is_ok_and(|id| self.in_grant(g, id)))
                    .map(|i| {
                        let runs = i.id.parse().is_ok_and(|id| self.runs(id, boss));
                        let mut r = json!({
                            "id": i.id, "name": i.name, "cwd": i.cwd,
                            "engine": i.engine.map(|e| e.as_str()),
                            "state": i.state, "task": i.task, "since": i.since,
                            "you_run_it": runs,
                            "asks": i.approval.is_some() || i.question.is_some(),
                        });
                        if shows(Part::Question) {
                            r["detail"] = json!(i.detail);
                            r["approval"] = json!(i.approval.as_ref().map(|a| (&a.id, &a.summary)));
                            r["question"] = json!(i.question.as_ref().map(|q| &q.text));
                        }
                        r
                    })
                    .collect();
                Ok(json!({
                    "sessions": reported(&Value::Array(rows).to_string(), "the group's sessions here"),
                    "slots": {
                        "busy": self.desk.charges_busy(boss),
                        "parallel": served_oversight(g).work.parallel,
                    },
                }))
            }
            "crew.activity" => {
                let limit = input["limit"].as_i64().unwrap_or(30).clamp(1, 100);
                let since = chrono::Utc::now() - chrono::Duration::days(7);
                let page = self
                    .desk
                    .store()
                    .events_since(since, &[], limit * 4, None)
                    .await
                    .map_err(internal)?;
                let lines: Vec<String> = page
                    .iter()
                    // About one of the grant's sessions, and written while it
                    // was in the grant's group: what it did in another group
                    // before it came stays that group's.
                    .filter(|e| {
                        e.subject
                            .as_ref()
                            .and_then(|s| s.id.parse().ok())
                            .is_some_and(|id| self.in_grant(g, id))
                            && e.group.as_deref() == stamped_as(g)
                    })
                    .take(limit as usize)
                    .map(|e| {
                        let about = e
                            .subject
                            .as_ref()
                            .map(|s| format!(" (about {} {})", s.name, s.id))
                            .unwrap_or_default();
                        match shows(Part::Words) {
                            true => format!(
                                "{} {} {} {}{}{}",
                                e.at.to_rfc3339(),
                                e.kind.as_str(),
                                e.actor.name,
                                e.text,
                                about,
                                e.reason
                                    .as_deref()
                                    .map(|r| format!(" — {r}"))
                                    .unwrap_or_default()
                            ),
                            false => format!(
                                "{} {} {}{about}",
                                e.at.to_rfc3339(),
                                e.kind.as_str(),
                                e.actor.name
                            ),
                        }
                    })
                    .collect();
                Ok(json!({ "events": reported(&lines.join("\n"), "the group's activity here") }))
            }
            "crew.history" => {
                let id: Uuid = input["session_id"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or("Say which session, by its id from crew.overview.")?;
                if !self.in_grant(g, id) {
                    return Err(self.not_in_grant(g));
                }
                let v = self.desk.view(id).map_err(|_| self.not_in_grant(g))?;
                let mut said = json!({
                    "name": v.label(), "goal": v.goal, "cwd": v.cwd, "state": v.state,
                });
                if shows(Part::Reply) {
                    said["last_prompt"] = json!(v.last_prompt);
                    said["last_reply"] = json!(v.last_reply);
                }
                if shows(Part::Question) {
                    said["pending"] = json!(v.pending);
                }
                if shows(Part::Terminal) {
                    said["terminal"] = json!(self
                        .desk
                        .terminal_text(id, sees::MAX_CHARS)
                        .ok()
                        .map(|t| sees::last_rows(&t)));
                }
                Ok(json!({ "session": reported(&said.to_string(), "the session") }))
            }
            _ => Err("not a reading tool".into()),
        }
    }

    async fn served_act(
        self: &Arc<Self>,
        tool: &'static str,
        input: &Value,
        g: &Grant,
        o: &Oversight,
        boss: &str,
        unbound: bool,
    ) -> Result<(ServedStatus, Value), String> {
        let reason = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or("Give the reason: the person reads it in Activity.")?;
        let mut kept = input.clone();
        if let Some(o) = kept.as_object_mut() {
            o.remove("ticket_id");
        }
        let action = Action::parse(tool, &kept)?;
        // What it names is this engine's session in the grant, or nothing
        // is said of it.
        match &action {
            Action::Nudge {
                to: Target::Agent(_),
                ..
            } => return Err("A boss on another hub messages sessions only.".into()),
            Action::Escalate { message, .. } => {
                let asked_by = self
                    .desk
                    .store()
                    .line_message(*message)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|m| m.from.id.parse::<Uuid>().ok());
                if !asked_by.is_some_and(|s| self.in_grant(g, s)) {
                    return Err(self.not_in_grant(g));
                }
            }
            Action::StartSession { .. } | Action::Release { .. } => {}
            other => {
                let s = Self::session_of(other).ok_or("That isn't served to a boss here.")?;
                if !self.in_grant(g, s) {
                    return Err(self.not_in_grant(g));
                }
            }
        }
        if let Action::Nudge {
            reply_to: Some(ask),
            ..
        } = &action
        {
            let of = self
                .desk
                .store()
                .line_message(*ask)
                .await
                .ok()
                .flatten()
                .and_then(|m| m.from.id.parse::<Uuid>().ok());
            if !of.is_some_and(|s| self.in_grant(g, s)) {
                return Err(self.not_in_grant(g));
            }
        }
        if let Action::Release { session } = &action {
            if !self.in_grant(g, *session) || !self.runs(*session, boss) {
                return Err("You don't run that session.".into());
            }
        }
        self.know_question(&action).await;
        // What no boss does, at every level short of unbound, before the
        // level is looked at.
        if let Some(n) = self.never_said(&action).filter(|_| !unbound) {
            if o.level == Level::Watch {
                return Err("You only watch: tell the person what you would do instead.".into());
            }
            return Ok(self.served_suggestion(
                &action,
                &format!(
                    "it {}; that is always the person's, at every level",
                    n.why()
                ),
            ));
        }
        // A boss on another hub acts only on the sessions it runs; on any
        // other, what it would do is the person's.
        if let Some(s) = Self::session_of(&action) {
            if !self.runs(s, boss) {
                if o.level == Level::Watch {
                    return Err("You only watch: tell the person what you would do instead.".into());
                }
                return Ok(self.served_suggestion(
                    &action,
                    "you don't run this session; the person decides it",
                ));
            }
        }
        // Unbound, what it does says so, in Activity and its decisions.
        let by = match unbound {
            true => By::manager(
                boss,
                &boss_name(g),
                &format!("{} (unbound: no limits)", reason.trim_end()),
            ),
            false => By::manager(boss, &boss_name(g), reason),
        };
        match self.gate_for(&action, o, Some(boss), unbound, true) {
            Gate::Refuse => {
                Err("You only watch the crew: tell the person what you would do instead.".into())
            }
            Gate::Suggest(why) => Ok(self.served_suggestion(&action, &why)),
            Gate::Run => {
                let done = match &action {
                    Action::StartSession { .. } => {
                        self.served_start(&action, g, o, boss, by).await?
                    }
                    _ => self.perform(&action, by).await?,
                };
                Ok((ServedStatus::Ok, json!({ "done": done })))
            }
        }
    }

    /// Where the boss served from a grant `boss` (`<hub>/<agent>`) acts
    /// fully now, judged afresh each time from its live grants here (and,
    /// for a boss relayed from the home, the level and switches its last
    /// call said it has there), by the one unbound check every boss goes
    /// through ([`Self::unbound_given`]): only when it is served (no local
    /// overseer of its group) and unbound by every grant that names it.
    /// Short of unbound, none: it acts within Desk.
    pub(crate) fn served_full(&self, boss: &str) -> Option<workforce_desk::charge::FullGrant> {
        if !grant::is_served(boss) {
            return None;
        }
        let ceiling = self.ceilings.lock().get(boss).cloned();
        let mut repos: Vec<String> = Vec::new();
        let mut any = false;
        for (g, not_live) in self.desk.grants_bossed() {
            let Some(b) = g.boss.as_ref().filter(|b| b.party_id() == boss) else {
                continue;
            };
            if not_live.is_some() {
                continue;
            }
            if !b.servable() {
                return None;
            }
            let o = match (b, &ceiling) {
                (_, Some(c)) => narrowed(served_oversight(&g), c),
                // Relayed from the home, and what it may do there not said
                // yet: not unbound until a call says.
                (BossRef::Local { .. }, None) => return None,
                (BossRef::Remote { .. }, None) => served_oversight(&g),
            };
            if self.overseer_for(&o.scope).is_some() || !self.unbound_given(boss, &o) {
                return None;
            }
            any = true;
            for r in &g.repos {
                if !repos.contains(r) {
                    repos.push(r.clone());
                }
            }
        }
        any.then(|| workforce_desk::charge::FullGrant {
            repos,
            workflows: Vec::new(),
            anywhere: true,
            unbound: true,
        })
    }

    /// Reads the real money the sessions of `boss`'s live grants at Act
    /// fully with a budget spent today here (calls a key paid for, never
    /// the person's plans), for [`Self::fully`]: one that can't be read
    /// leaves none, so Act fully waits.
    pub(crate) async fn read_served_spent(&self, boss: &str) {
        let now = chrono::Utc::now();
        let midnight = self
            .desk
            .period_start(workforce_desk::ledger::Period::Day, now);
        let mut total = 0.0;
        let mut any = false;
        for (g, not_live) in self.desk.grants_bossed() {
            if not_live.is_some() || g.boss.as_ref().map(BossRef::party_id).as_deref() != Some(boss)
            {
                continue;
            }
            let o = served_oversight(&g);
            if o.level != Level::Full || o.full.budget_cents_per_day.is_none() {
                continue;
            }
            let filter = workforce_desk::ledger_store::Filter {
                seen: Some(self.desk.seen_by(&o.scope)),
                ..Default::default()
            };
            match self.desk.billed_usd(midnight, now, &filter).await {
                Ok(usd) => {
                    total += usd;
                    any = true;
                }
                Err(e) => {
                    tracing::warn!(%boss, error = %e, "couldn't read a served boss's spend today; Act fully waits until it can");
                    self.spent_today.lock().remove(boss);
                    return;
                }
            }
        }
        let mut today = self.spent_today.lock();
        match any {
            true => today.insert(boss.to_string(), total),
            false => today.remove(boss),
        };
    }

    fn served_suggestion(&self, action: &Action, why: &str) -> (ServedStatus, Value) {
        (
            ServedStatus::Suggested,
            json!({
                "suggested": self.describe(action),
                "note": format!("Not done: {why}. It goes to the person to decide."),
            }),
        )
    }

    /// Starts the session `action` asks for, for the served boss: only in a
    /// folder the grant names, only on an engine whose every call Desk
    /// checks (and refuses when it can't), in that engine's own mode, with
    /// nothing of the person's (no approvals said for it, no environment,
    /// no account, no worktree), in the grant's group.
    async fn served_start(
        &self,
        action: &Action,
        g: &Grant,
        o: &Oversight,
        boss: &str,
        by: By,
    ) -> Result<String, String> {
        let Action::StartSession {
            folder,
            goal,
            name,
            engine,
            view,
            worktree,
            ..
        } = action
        else {
            return Err("That isn't a session to start.".into());
        };
        // What becomes its launch says nothing that reads as an option.
        if goal.trim_start().starts_with('-')
            || name
                .as_deref()
                .is_some_and(|n| n.trim_start().starts_with('-'))
        {
            return Err("A session's goal and name can't start with \"-\".".into());
        }
        if worktree.is_some() {
            return Err(
                "A boss on another hub starts sessions in the grant's folders themselves, not in a worktree."
                    .into(),
            );
        }
        let Some(folder) = folder_inside(folder, &g.folders) else {
            return Err(format!(
                "{folder} isn't one of {}'s folders here: start it in one of those.",
                g.name
            ));
        };
        // Started where it was checked, as the disk spells it.
        let folder = &folder.to_string_lossy().into_owned();
        let engine = engine.unwrap_or(Engine::Claude);
        if !workforce_desk::engine::boss_safe(engine) {
            return Err(format!(
                "A boss on another hub runs only sessions Desk checks every call of, so not {}.",
                engine.title()
            ));
        }
        let view = self.startable(engine, *view)?;
        let party = Party::manager(boss, &boss_name(g));
        let _one = self.starting.lock().await;
        self.slot_free(&party, o, &by)?;
        let label = name.clone().unwrap_or_else(|| {
            std::path::Path::new(folder)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".into())
        });
        let v = self
            .desk
            .create_for(
                NewSession {
                    fresh_worktree: false,
                    engine,
                    view: Some(view),
                    name: label,
                    cwd: folder.clone(),
                    goal: goal.clone(),
                    worktree: None,
                    prompt: Some(goal.clone()),
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    account: None,
                    env: Default::default(),
                    group: (g.group != workforce_desk::UNGROUPED).then(|| g.group.clone()),
                },
                &party,
                by,
            )
            .await
            .map_err(|e| match e {
                workforce_desk::DeskError::Internal(e) => {
                    tracing::warn!(error = %e, "a served session didn't start");
                    "Something went wrong on this engine.".to_string()
                }
                other => other.to_string(),
            })?;
        Ok(format!("started {} ({}); you run it", v.label(), v.id))
    }
}

/// What a served boss is called here: its agent, of its hub.
fn boss_name(g: &Grant) -> String {
    match &g.boss {
        Some(BossRef::Remote { hub, agent }) => format!("{agent} ({hub})"),
        Some(BossRef::Local { agent }) => agent.clone(),
        None => "a boss".into(),
    }
}

/// While it lives, its remote bosses are connected
/// ([`CrewHub::remote_up`]).
pub(crate) struct RemoteUp {
    crew: Arc<CrewHub>,
    bosses: Vec<String>,
}

impl Drop for RemoteUp {
    fn drop(&mut self) {
        let mut up = self.crew.remote_up.lock();
        for b in &self.bosses {
            if let Some(n) = up.get_mut(b) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    up.remove(b);
                }
            }
        }
    }
}

/// How long a request given to a remote boss may stay open while the boss
/// has it before it is the person's: its gathering, its turn and a margin.
/// The boss's turn ends on its own hub, out of this engine's sight.
pub(crate) const LEFT_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A request given to a remote boss: when, and whether it has since been
/// left to the person.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Offered {
    pub(crate) at: std::time::Instant,
    pub(crate) left: bool,
}
