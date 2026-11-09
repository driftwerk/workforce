//! Continue in another engine (session memory, SM6): a session's work goes
//! on in a session of another engine (or the same one, afresh), which is
//! given a brief of it, and the two are one piece of work from then.
//!
//! The facts every check reads come from the registry, the sidebar and
//! session memory, never from what was asked: the session it starts is in
//! the source's group, a session handed back to must still be in it, and
//! the brief is read only from what was kept while the source was in that
//! group (each row checked by [`may_see`] as a session of that group reads
//! it). Everything recalled into it is wrapped as untrusted content with a
//! fresh nonce each time it is read, and what wasn't harvested (the gist,
//! the goal, the git state) is redacted as the harvest's text is.
//!
//! On a hub that may be a machine's, every handoff is refused: the groups
//! that bound a brief are kept by the Desk that drives it, not there.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::crew::{By, Kind, Party};
use crate::engine::{self, Deliver, Engine, View};
use crate::floor::{SubjectKey, TurnOp};
use crate::handoff::{self as rules, After, Ask, Back, Content, Facts, Lineage, Link, Place};
use crate::line::{Delivery, LineKind, LineMessage};
use crate::lineage_store::{KeptLineage, LineageWrite};
use crate::memory::brief::{
    self, BriefExchange, BriefInput, BriefItem, GitState, Included, SourceInfo,
};
use crate::memory::reach::{bound, may_see, Reach, Reader, Row};
use crate::memory_store::group_key;
use crate::model::{NewSession, SessionRecord, SessionState, SessionView};

/// The most exchanges a brief is written from: older ones are counted,
/// and left to `recall`.
const EXCHANGES_READ: i64 = 400;
/// How long a source asked to stop once its turn ends is waited on.
const AFTER_TURN_WAIT: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a target given its brief by typing is waited on to be idle.
const PASTE_WAIT: Duration = Duration::from_secs(120);

/// Handoff's lineage as kept, and how long a preview waits for Desk's notes.
pub(super) struct HandoffRun {
    pub(super) lineage: Mutex<Lineage>,
    pub(super) wait: Mutex<Duration>,
    /// Held while a handoff is made: one at a time, so a work is never
    /// handed on twice.
    pub(super) write: tokio::sync::Mutex<()>,
}

impl Default for HandoffRun {
    fn default() -> Self {
        Self {
            lineage: Mutex::default(),
            wait: Mutex::new(crate::gist::WRITE_WITHIN),
            write: tokio::sync::Mutex::new(()),
        }
    }
}

/// A session's place in its work, as its card shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageView {
    /// The work's first session.
    pub work: Uuid,
    /// Where it came from, when it was handed the work.
    pub from: Option<Hop>,
    /// Where it went, when the work continues elsewhere.
    pub handed_to: Option<Hop>,
    /// Every session of the work, the first first.
    pub sessions: Vec<Uuid>,
}

/// One step of a work's lineage.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hop {
    pub session: Uuid,
    pub name: String,
    pub engine: Engine,
    pub at: Option<DateTime<Utc>>,
}

/// What a handoff would give, before the person confirms it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffPreview {
    pub engine: Engine,
    pub content: Content,
    /// The brief as the target would read it.
    pub brief: String,
    pub included: Included,
    /// What it holds, in the person's words: "Codex gets: …".
    pub gets: String,
    /// What it leaves out, when anything.
    pub leaves_out: Option<String>,
    pub tokens: u32,
    pub budget: u32,
    /// Who receives the brief, and the words that say so.
    pub vendor: Option<&'static str>,
    pub egress: String,
    /// The source's group asks the person to confirm each handoff.
    pub ask: bool,
    /// The target can search the rest of the work.
    pub search: bool,
    /// Said when it can't: "Only the brief goes across."
    pub only_brief: Option<&'static str>,
    /// Desk's notes of the source weren't ready in time.
    pub notes_pending: bool,
    /// A hand back is offered: resume the session the work came from.
    pub back: Option<Hop>,
    pub lineage: LineageView,
}

/// A handoff made.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandedOff {
    /// The session the work continues in.
    pub session: SessionView,
    pub included: Included,
    /// "Handed over: …", as the target's card says it.
    pub said: String,
    pub egress: String,
    /// What else the person should know: what didn't follow the work.
    pub notes: Vec<String>,
}

/// "Handed over: the goal, 7 decisions … The rest can be searched."
fn handed_over(i: &Included, search: bool) -> String {
    let rest = match search {
        true => " The rest can be searched.",
        false => " Only the brief goes across.",
    };
    format!("Handed over: {}.{rest}", brief::holds(i))
}

impl Desk {
    /// Loads handoff's lineage as kept.
    pub(super) async fn lineage_load(&self) {
        match self.0.store.lineage().await {
            Ok((links, handed)) => {
                let mut l = self.0.handoff.lineage.lock();
                l.links = links.into_iter().collect();
                l.handed_to = handed.into_iter().collect();
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read which sessions are one piece of work"
                );
            }
        }
    }

    /// Session `id` is being forgotten: its work goes back to the sessions
    /// it came from, which can be handed on again, and it holds none.
    pub(super) async fn lineage_forgotten(&self, id: Uuid) {
        if let Err(e) = self.0.store.forget_lineage(id).await {
            tracing::warn!(session = %id, error = format!("{e:#}"), "couldn't take work back from a forgotten session");
        }
        self.0.handoff.lineage.lock().forget(id);
    }

    /// Session `id` is being forgotten: what its work was handed on with
    /// goes from the Floor's gists of every session it reached, down the
    /// chain, unless one wrote its own since. What the person should know:
    /// those sessions' own conversations may still hold what they read.
    pub(super) fn carried_forgotten(&self, id: Uuid) -> Option<String> {
        let reached = self.0.handoff.lineage.lock().carried_from(id);
        if reached.is_empty() {
            return None;
        }
        self.log_turns(vec![TurnOp::ForgetCarried {
            from: SubjectKey::session(id),
            to: reached.iter().copied().map(SubjectKey::session).collect(),
        }]);
        let names: Vec<String> = reached
            .iter()
            .map(|s| {
                self.record(*s).map_or_else(
                    |_| "a session since forgotten".into(),
                    |r| named(&r).to_string(),
                )
            })
            .collect();
        let one = names.len() == 1;
        Some(format!(
            "Its work was handed on to {}. Desk forgets what it gave {}, but {} own conversation may still hold what {} read.",
            names.join(", "),
            if one { "it" } else { "them" },
            if one { "its" } else { "their" },
            if one { "it" } else { "they" },
        ))
    }

    /// Handoff's lineage as it is now.
    pub(super) fn lineage(&self) -> Lineage {
        self.0.handoff.lineage.lock().clone()
    }

    /// Every session of `id`'s work, itself among them.
    pub(super) fn work_sessions(&self, id: Uuid) -> Vec<Uuid> {
        self.0.handoff.lineage.lock().sessions(id)
    }

    /// How long a preview waits for Desk's notes of its source.
    #[cfg(test)]
    pub(super) fn set_handoff_wait(&self, wait: Duration) {
        *self.0.handoff.wait.lock() = wait;
    }

    /// A session's name and engine, as Desk has it or as memory kept it.
    async fn hop(&self, id: Uuid, at: Option<DateTime<Utc>>) -> Hop {
        if let Ok(r) = self.record(id) {
            return Hop {
                session: id,
                name: named(&r).to_string(),
                engine: r.engine,
                at,
            };
        }
        let kept = self
            .0
            .store
            .memory_names(&[id])
            .await
            .ok()
            .unwrap_or_default();
        let (name, engine) = kept.into_iter().next().map_or(
            ("a session since forgotten".to_string(), Engine::Custom),
            |(_, n, e)| (n, e),
        );
        Hop {
            session: id,
            name,
            engine,
            at,
        }
    }

    /// `id`'s place in its work.
    pub async fn session_lineage(&self, id: Uuid) -> LineageView {
        let l = self.lineage();
        let from = match l.links.get(&id) {
            Some(link) => Some(self.hop(link.from, Some(link.at)).await),
            None => None,
        };
        let handed_to = match l.handed_to.get(&id) {
            Some(to) => {
                let at = l.links.get(to).map(|k| k.at);
                Some(self.hop(*to, at).await)
            }
            None => None,
        };
        LineageView {
            work: l.work_of(id),
            from,
            handed_to,
            sessions: l.sessions(id),
        }
    }

    /// `view` with each piece of work on one tile ([`crate::floor::fold_work`]):
    /// a session whose work went on in another is folded under it, and the
    /// one it went on in says where from. For the person's Floor, whose
    /// `day` it is.
    pub fn fold_floor(&self, view: &mut crate::floor::FloorView, day: DateTime<Utc>) {
        let l = self.lineage();
        let handed: std::collections::HashSet<Uuid> = l.handed_to.keys().copied().collect();
        let from = l
            .links
            .iter()
            .filter(|(s, _)| !l.handed_off(**s))
            .map(|(s, link)| {
                (
                    *s,
                    crate::floor::Continued {
                        from: link.from,
                        engine: self.record(link.from).ok().map(|r| r.engine),
                        at: link.at,
                    },
                )
            })
            .collect();
        crate::floor::fold_work(view, &handed, &from, day);
    }

    /// The facts a handoff of `r` as `ask` asks is checked against, and
    /// the session a hand back resumes.
    async fn handoff_facts(
        &self,
        r: &SessionRecord,
        ask: &Ask,
        target: Engine,
    ) -> (Facts, Option<SessionRecord>) {
        let lineage = self.lineage();
        let group_of = |id: Uuid| {
            self.group_of_member(&session_item(id))
                .filter(|g| g != UNGROUPED)
        };
        let source_group = group_of(r.id);
        let (back, back_record, target_group) = match ask.back {
            false => (None, None, source_group.clone()),
            true => {
                let came_from = lineage
                    .links
                    .get(&r.id)
                    .map(|l| l.from)
                    .filter(|from| lineage.handed_to.get(from) == Some(&r.id));
                let came = match came_from {
                    Some(from) => self.load(from).await.ok(),
                    None => None,
                };
                match came {
                    Some(s) => {
                        let running = self.0.state.lock().live.contains_key(&s.id);
                        let resumable = engine::caps(s.engine, s.view).resume
                            == engine::Resume::ById
                            && s.cli_session_id.is_some();
                        (
                            Some(Back {
                                engine: s.engine,
                                resumable,
                                running,
                            }),
                            Some(s.clone()),
                            group_of(s.id),
                        )
                    }
                    None => (None, None, source_group.clone()),
                }
            }
        };
        let signed_in = match ask.back {
            true => true,
            false => match self.account_for(target, ask.account) {
                Err(_) => false,
                Ok(Some(a)) => self.accounts().iter().any(|v| v.id == a && v.signed_in),
                Ok(None) => !matches!(self.sign_ins().of(target), engine::SignIn::Out),
            },
        };
        let in_terminal = self
            .0
            .state
            .lock()
            .live
            .get(&r.id)
            .is_some_and(|l| l.terminal.is_some());
        let facts = Facts {
            machine: self.0.opts.memory.machine,
            source_engine: r.engine,
            hub_internal: self.0.opts.hub_internal.holds(Path::new(&r.cwd))
                || crate::ledger::agent_turn(&r.cwd, &r.name)
                || Self::hubs_own(&self.0.opts.hub_internal, r),
            memory_on: true,
            record: matches!(r.engine, Engine::Claude | Engine::Codex),
            redactor: self.0.ledger.redactor.is_some(),
            source_group,
            handed_to: lineage.handed_to.get(&r.id).copied(),
            target,
            target_group,
            signed_in,
            back,
            in_terminal,
        };
        (facts, back_record)
    }

    /// The engine a handoff starts: what was asked, else (handing back)
    /// the engine of the session it resumes.
    async fn handoff_target(&self, id: Uuid, ask: &Ask) -> Result<Engine, DeskError> {
        if let Some(e) = ask.engine {
            return Ok(e);
        }
        if ask.back {
            let l = self.lineage();
            if let Some(from) = l.links.get(&id).map(|k| k.from) {
                if let Ok(r) = self.load(from).await {
                    return Ok(r.engine);
                }
            }
            return Err(DeskError::Conflict(
                rules::Refusal::NothingToHandBack.to_string(),
            ));
        }
        Err(DeskError::Invalid(
            "Say which engine to continue in.".into(),
        ))
    }

    /// Whether `target` sessions started now have Desk's session tools.
    fn has_tools(&self, target: Engine) -> bool {
        let version = self.version_of(target);
        engine::session_caps(target, View::Terminal, version.as_deref(), false).line
            != Deliver::None
    }

    /// Text that wasn't harvested, as the harvest would keep it.
    fn redacted(&self, text: Option<&str>) -> Option<String> {
        let text = text.map(str::trim).filter(|t| !t.is_empty())?;
        let r = self.0.ledger.redactor.as_deref()?;
        Some(r.stored(text).text.into_owned())
    }

    /// What a brief read under `group` holds of `r` for `target`, and the
    /// tokens it may take: read from `since` for a hand back.
    #[allow(clippy::too_many_arguments)]
    async fn write_brief(
        &self,
        r: &SessionRecord,
        group: Option<&str>,
        target: Engine,
        content: Content,
        place: Place,
        since: Option<DateTime<Utc>>,
        notes_pending: bool,
        nonce: &str,
    ) -> Result<(brief::Brief, u32), DeskError> {
        let reader = Reader::Session {
            id: r.id,
            group: group.map(str::to_string),
            project: self.project_of(r).await,
            work: self.work_sessions(r.id),
        };
        let known = self.0.store.memory_groups().await?;
        // The source's own rows, as one of its group reads them: what was
        // kept while it was in another group is never the brief's.
        let b = bound(&reader, Reach::Group, None, &known);
        let sees = |g: Option<&str>, session: Uuid, project: &str, at| {
            session == r.id
                && may_see(
                    &reader,
                    &b,
                    &Row {
                        group: g,
                        session,
                        project,
                        at,
                    },
                )
        };
        let mut items = Vec::new();
        let mut exchanges = Vec::new();
        let (mut elsewhere, mut earlier, mut first_ask) = (0, 0, None);
        if content == Content::Brief {
            for i in self.0.store.session_items(r.id).await? {
                if !sees(i.group.as_deref(), i.session, &i.project, i.valid_from) {
                    continue;
                }
                // A hand back gives only what is new.
                if since.is_some_and(|s| i.valid_from < s) {
                    continue;
                }
                items.push(BriefItem {
                    kind: i.kind,
                    state: i.state,
                    confirmed: i.confirmed || i.by == "person",
                    noted: i.by != "person",
                    reference: format!("i-{}", i.id),
                    at: i.valid_from,
                    text: i.text,
                });
            }
            let (rows, all) = self
                .0
                .store
                .brief_exchanges(r.id, since, EXCHANGES_READ)
                .await?;
            earlier = usize::try_from(all).unwrap_or(0).saturating_sub(rows.len());
            for x in rows {
                if !sees(x.group.as_deref(), x.session, &x.project, x.exchange.at) {
                    elsewhere += 1;
                    continue;
                }
                let k = x.exchange;
                exchanges.push(BriefExchange {
                    reference: format!("x-{}", x.id),
                    at: k.at,
                    asker: k.asker,
                    ask: k.ask,
                    answer: k.answer,
                    steps: k.steps,
                    files: k.files,
                    commands: k.commands,
                    errors: k.errors,
                    compaction: k.compaction,
                    flagged: x.flagged,
                });
            }
            if earlier == 0 && since.is_none() {
                first_ask = exchanges
                    .iter()
                    .find(|x| !x.compaction && !x.flagged)
                    .map(|x| x.ask.clone());
            }
        }
        let (purpose, now) = match (&self.0.derive.deriver, since, content) {
            // A hand back's source began as its brief: its purpose is the
            // work's, which the session resumed already knows. Only the
            // goal goes with nothing but the goal: the gist is read from
            // the conversation that can't be handed over.
            (Some(d), None, Content::Brief) => d.gist(r.id).await,
            _ => (None, None),
        };
        let (purpose, now, goal) = (
            self.redacted(purpose.as_deref()),
            self.redacted(now.as_deref()),
            match since {
                None => self.redacted(Some(&r.goal)),
                Some(_) => None,
            },
        );
        let git = match content {
            Content::Brief => {
                let (branch, head, dirty) = crate::worktree::state_of(Path::new(&r.cwd)).await;
                GitState {
                    branch: self.redacted(branch.as_deref()),
                    head,
                    dirty: dirty
                        .iter()
                        .filter_map(|d| self.redacted(Some(d)))
                        .collect(),
                    new_worktree: place == Place::NewWorktree,
                }
            }
            Content::GoalOnly => GitState::default(),
        };
        let source = SourceInfo {
            engine: r.engine,
            name: named(r).to_string(),
            folder: r.cwd.clone(),
        };
        let caps = engine::caps(target, View::Terminal);
        let budget = brief::budget(
            engine::window_or_default(target, &caps),
            self.0.opts.memory.brief_max_tokens,
        );
        let input = BriefInput {
            purpose: purpose.as_deref(),
            now: now.as_deref(),
            goal: goal.as_deref(),
            first_ask: first_ask.as_deref(),
            items: &items,
            exchanges: &exchanges,
            elsewhere,
            earlier,
            git: &git,
            source: &source,
            target,
            tools: self.has_tools(target),
            since,
            notes_pending,
            at: Utc::now(),
        };
        Ok((brief::brief(&input, budget, nonce), budget))
    }

    /// Whether Desk may note decisions from `id` in `group` now.
    async fn notes_allowed(&self, id: Uuid, group: Option<&str>) -> Result<bool, DeskError> {
        if !self.0.opts.memory.derive || self.0.derive.deriver.is_none() {
            return Ok(false);
        }
        let f = self.0.store.derive_facts(id, group).await?;
        Ok(!f.paused && !f.session_off && f.group_on)
    }

    /// Has Desk note what it hasn't of `id` now, and waits for it up to
    /// the handoff's wait. Whether some is still unnoted.
    async fn notes_now(&self, id: Uuid, group: Option<&str>) -> Result<bool, DeskError> {
        if !self.notes_allowed(id, group).await? {
            return Ok(false);
        }
        if !self.0.store.derive_due_now(id).await? {
            return Ok(false);
        }
        self.derive_poke();
        let until = tokio::time::Instant::now() + *self.0.handoff.wait.lock();
        loop {
            let q = self.0.store.derive_queued(id).await?;
            if q.unread == 0 || q.failed.is_some() {
                return Ok(q.unread > 0);
            }
            if tokio::time::Instant::now() >= until {
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Whether Desk would note more of `id` than it has, without asking
    /// it to.
    async fn notes_unwritten(&self, id: Uuid, group: Option<&str>) -> Result<bool, DeskError> {
        if !self.notes_allowed(id, group).await? {
            return Ok(false);
        }
        let q = self.0.store.derive_queued(id).await?;
        Ok(q.unread > 0 && q.failed.is_none())
    }

    /// Whether `group`'s switches ask the person before each handoff.
    async fn asks_each_time(&self, group: Option<&str>) -> Result<bool, DeskError> {
        let Some(g) = group else {
            return Ok(false);
        };
        let set = self.0.store.setting(&group_key(g)).await?;
        Ok(super::memory::GroupMemory::of(set.as_ref()).handoff_ask)
    }

    /// The facts, the target and what goes, for a handoff of `id` as `ask`
    /// asks; `ask` with what becomes of the source said.
    async fn handoff_checked(&self, id: Uuid, ask: &Ask) -> Result<Checked, DeskError> {
        if self.0.opts.memory.machine {
            return Err(DeskError::Conflict(crate::memory::MACHINE_HANDOFF.into()));
        }
        let r = self.load(id).await?;
        let target = self.handoff_target(id, ask).await?;
        let (in_terminal, working) = {
            let st = self.0.state.lock();
            (
                st.live.get(&id).is_some_and(|l| l.terminal.is_some()),
                st.live.contains_key(&id)
                    && matches!(
                        r.state,
                        SessionState::Starting
                            | SessionState::Working
                            | SessionState::Blocked
                            | SessionState::Asking
                    ),
            )
        };
        let mut ask = ask.clone();
        ask.source = Some(ask.source.unwrap_or(match in_terminal {
            true => After::Keep,
            false => After::default_for(working),
        }));
        let (mut facts, back) = self.handoff_facts(&r, &ask, target).await;
        facts.memory_on = self.0.store.session_memory_on(id).await?.unwrap_or(true);
        let content =
            rules::check(&facts, &ask).map_err(|why| DeskError::Conflict(why.to_string()))?;
        let since = match ask.back {
            true => self.lineage().links.get(&id).map(|l| l.at),
            false => None,
        };
        Ok(Checked {
            r,
            target,
            facts,
            content,
            back,
            ask,
            since,
        })
    }

    /// What a handoff of `id` as `ask` asks would give: its brief and what
    /// the person reads of it. With `derive`, Desk notes what it hasn't of
    /// the source first, waiting a while for it: only when the person asks
    /// for the preview, never for a prefetch, which spends no model call.
    pub async fn handoff_preview(
        &self,
        id: Uuid,
        ask: &Ask,
        derive: bool,
    ) -> Result<HandoffPreview, DeskError> {
        let c = self.handoff_checked(id, ask).await?;
        let group = c.facts.source_group.as_deref();
        let notes_pending = match (c.content, derive) {
            (Content::Brief, true) => self.notes_now(id, group).await?,
            (Content::Brief, false) => self.notes_unwritten(id, group).await?,
            (Content::GoalOnly, _) => false,
        };
        let (b, budget) = self
            .write_brief(
                &c.r,
                group,
                c.target,
                c.content,
                c.ask.place,
                c.since,
                notes_pending,
                &nonce(),
            )
            .await?;
        let search = self.has_tools(c.target);
        let back = match &c.back {
            Some(s) => Some(self.hop(s.id, None).await),
            None => None,
        };
        // Offered on the session the work was handed to: back to where it
        // came from.
        let back = match back {
            Some(b) => Some(b),
            None => {
                let l = self.lineage();
                match l.links.get(&id).map(|k| k.from) {
                    Some(from) if l.handed_to.get(&from) == Some(&id) => {
                        let resumable = self.load(from).await.is_ok_and(|s| {
                            engine::caps(s.engine, s.view).resume == engine::Resume::ById
                                && s.cli_session_id.is_some()
                        });
                        match resumable {
                            true => Some(self.hop(from, None).await),
                            false => None,
                        }
                    }
                    _ => None,
                }
            }
        };
        Ok(HandoffPreview {
            engine: c.target,
            content: c.content,
            gets: format!("{} gets: {}.", c.target.title(), brief::holds(&b.included)),
            leaves_out: brief::leaves_out(&b.included),
            tokens: b.included.tokens,
            budget,
            vendor: engine::vendor_of(c.target),
            egress: rules::egress(c.target),
            ask: self.asks_each_time(group).await?,
            search,
            only_brief: (!search).then_some("Only the brief goes across."),
            notes_pending,
            back,
            lineage: self.session_lineage(id).await,
            included: b.included,
            brief: b.text,
        })
    }

    /// Hands `id`'s work on as `ask` asks, as `by`: starts the session that
    /// continues it (or resumes the one it came from) with only a short
    /// prompt, its brief kept for it first.
    pub async fn handoff(&self, id: Uuid, ask: &Ask, by: By) -> Result<HandedOff, DeskError> {
        let _one = self.0.handoff.write.lock().await;
        let c = self.handoff_checked(id, ask).await?;
        let group = c.facts.source_group.clone();
        let egress = rules::egress(c.target);
        if self.asks_each_time(group.as_deref()).await? && !c.ask.egress_confirmed {
            return Err(DeskError::Conflict(format!(
                "{egress} This group asks before each handoff: confirm it to go on."
            )));
        }
        let notes_pending = match c.content {
            Content::Brief => self.notes_unwritten(id, group.as_deref()).await?,
            Content::GoalOnly => false,
        };
        let (b, _) = self
            .write_brief(
                &c.r,
                group.as_deref(),
                c.target,
                c.content,
                c.ask.place,
                c.since,
                notes_pending,
                &nonce(),
            )
            .await?;
        let tools = self.has_tools(c.target);
        let work = self.lineage().work_of(id);
        let link = Link {
            work,
            from: id,
            at: Utc::now(),
            engine: c.target,
            group: group.clone(),
            included: b.included,
        };
        let view = match &c.back {
            None => self.handoff_start(&c, &link, &b.text, tools, &by).await?,
            Some(s) => self.handoff_back(&c, s, &link, &b.text, tools, &by).await?,
        };
        let back_to = c.back.as_ref().map(|s| s.id);
        if let Err(e) = self.0.store.hand_on(id, Some(view.id), back_to).await {
            tracing::warn!(session = %id, error = format!("{e:#}"), "couldn't keep where a session's work continues");
        }
        self.0.handoff.lineage.lock().hand_on(id, view.id, link);
        let source = self.view(id)?;
        self.note(by.event(
            Kind::HandedOff,
            Some(Party::session(&source)),
            match back_to {
                None => format!(
                    "handed {} on to {} ({})",
                    source.label(),
                    view.label(),
                    c.target.title()
                ),
                Some(_) => format!(
                    "handed {} back to {} ({})",
                    source.label(),
                    view.label(),
                    c.target.title()
                ),
            },
        ));
        let mut notes = self.work_follows(&source, &view, &by).await;
        notes.extend(self.source_after(id, c.ask.source.unwrap_or(After::Keep), &by));
        if !tools {
            self.paste_brief(view.id, b.text.clone());
        }
        Ok(HandedOff {
            session: self.view(view.id)?,
            said: handed_over(&b.included, tools),
            included: b.included,
            egress,
            notes,
        })
    }

    /// Starts the session that continues `c`'s work, its lineage and brief
    /// kept before it runs: the first thing it does is read them.
    async fn handoff_start(
        &self,
        c: &Checked,
        link: &Link,
        brief: &str,
        tools: bool,
        by: &By,
    ) -> Result<SessionView, DeskError> {
        let r = &c.r;
        let (cwd, branch) = match c.ask.place {
            Place::SameFolder => (r.cwd.clone(), None),
            Place::NewWorktree => {
                let (now_on, head, _) = crate::worktree::state_of(Path::new(&r.cwd)).await;
                let head = head.ok_or_else(|| {
                    DeskError::Invalid(format!(
                        "{} isn't in a git repository, so it has no worktree to branch from.",
                        r.cwd
                    ))
                })?;
                let name = rules::branch_for(now_on.as_deref(), &r.cwd, c.target);
                let path = crate::worktree::create_at(Path::new(&r.cwd), &name, &head)
                    .await
                    .map_err(|e| DeskError::Invalid(e.to_string()))?;
                (path.to_string_lossy().into_owned(), Some(name))
            }
        };
        let id = Uuid::new_v4();
        let account = self.account_for(c.target, c.ask.account)?;
        self.still_in_group(c, &[r.id])?;
        self.0
            .store
            .put_lineage(&LineageWrite {
                session: id,
                link,
                account,
                brief: Some(brief),
                included: &link.included,
            })
            .await?;
        let new = NewSession {
            engine: c.target,
            view: None,
            name: r.name.clone(),
            cwd,
            goal: r.goal.clone(),
            worktree: None,
            prompt: Some(brief::short_prompt(r.engine, named(r), false, tools)),
            approvals: None,
            approve_for_me: false,
            account: c.ask.account,
            env: Default::default(),
            group: c.facts.source_group.clone(),
        };
        let started = self
            .start_session(new, Some(id), None, Some(by.clone()), r.folder_chosen)
            .await;
        match started {
            Ok(view) => {
                if let Some(b) = branch {
                    self.change(id, |r, _| r.branch = Some(b));
                }
                Ok(view)
            }
            Err(e) => {
                if let Err(undo) = self.0.store.restore_lineage(id, None).await {
                    tracing::warn!(session = %id, error = format!("{undo:#}"), "a handoff that didn't start keeps its brief until it is forgotten");
                }
                Err(e)
            }
        }
    }

    /// Refuses when any of `sessions` left the group `c` was checked in
    /// since: read again just before the lineage is written, so a move
    /// made meanwhile never takes a brief across groups.
    fn still_in_group(&self, c: &Checked, sessions: &[Uuid]) -> Result<(), DeskError> {
        let moved = sessions.iter().any(|s| {
            self.group_of_member(&session_item(*s))
                .filter(|g| g != UNGROUPED)
                != c.facts.source_group
        });
        match moved {
            true => Err(DeskError::Conflict(rules::Refusal::CrossGroup.to_string())),
            false => Ok(()),
        }
    }

    /// Resumes `back`, the session `c`'s work came from, with what happened
    /// since: its lineage and brief kept first, put back if it doesn't
    /// resume.
    async fn handoff_back(
        &self,
        c: &Checked,
        back: &SessionRecord,
        link: &Link,
        brief: &str,
        tools: bool,
        by: &By,
    ) -> Result<SessionView, DeskError> {
        let was: Option<KeptLineage> = self.0.store.lineage_row(back.id).await?;
        self.still_in_group(c, &[c.r.id, back.id])?;
        self.0
            .store
            .put_lineage(&LineageWrite {
                session: back.id,
                link,
                account: back.account,
                brief: Some(brief),
                included: &link.included,
            })
            .await?;
        let prompt = brief::short_prompt(c.r.engine, named(&c.r), true, tools);
        match self.resume(back.id, Some(prompt), Some(by.clone())).await {
            Ok(view) => Ok(view),
            Err(e) => {
                if let Err(undo) = self.0.store.restore_lineage(back.id, was.as_ref()).await {
                    tracing::warn!(session = %back.id, error = format!("{undo:#}"), "a hand back that didn't resume keeps its brief");
                }
                Err(e)
            }
        }
    }

    /// What follows the work from `from` to `to`: its purpose on the
    /// Floor, what waits on its line, its ticket and its own budgets. What
    /// the person should know of what didn't.
    async fn work_follows(&self, from: &SessionView, to: &SessionView, by: &By) -> Vec<String> {
        let mut notes = Vec::new();
        self.log_turns(vec![TurnOp::CarryPurpose {
            from: SubjectKey::session(from.id),
            to: SubjectKey::session(to.id),
        }]);
        // Its line: what waits for it goes to where its work is.
        match self
            .0
            .store
            .line_readdress(&from.id.to_string(), &Party::session(to))
            .await
        {
            Ok(moved) if !moved.is_empty() => {
                for m in &moved {
                    self.line_announce(m);
                }
                let mut note = LineMessage::new(
                    Party::desk(),
                    Party::session(from),
                    LineKind::Tell,
                    format!(
                        "This work continues in {} ({}), so the {} waiting here went to it.",
                        to.label(),
                        to.engine.title(),
                        match moved.len() {
                            1 => "message".to_string(),
                            n => format!("{n} messages"),
                        }
                    ),
                    0,
                );
                note.delivery = Delivery::Noted;
                note.group = self.line_group(&note);
                match self.0.store.put_line(&note).await {
                    Ok(()) => self.line_announce(&note),
                    Err(e) => tracing::warn!(
                        error = format!("{e:#}"),
                        "couldn't note on a thread where its messages went"
                    ),
                }
                self.line_went_live(to.id);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(session = %from.id, error = format!("{e:#}"), "a handed-off session's messages stayed with it");
                notes.push("Messages waiting for it couldn't be moved; they stay with it.".into());
            }
        }
        // Its ticket: the work's, so its holder is where the work is.
        match self.0.store.session_tickets(from.id).await {
            Ok(tickets) => {
                for t in tickets {
                    if let Err(e) = self.ticket_follows(t, to, by).await {
                        tracing::info!(session = %from.id, error = %e, "a ticket didn't follow its work");
                        notes.push(format!("Its ticket stayed with it ({e})."));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(session = %from.id, error = format!("{e:#}"), "couldn't read a handed-off session's tickets");
                notes.push("Its ticket couldn't be read, so it stays with it.".into());
            }
        }
        // Its own budget: the work's, wherever it runs.
        match self.0.store.move_session_budgets(from.id, to.id).await {
            Ok(moved) if !moved.is_empty() => {
                let text = format!(
                    "moved {}'s budget to {}, where its work continues",
                    from.label(),
                    to.label()
                );
                self.budgets_changed(by, text).await;
                notes.push(format!("Its budget moved to {} with the work.", to.label()));
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(session = %from.id, error = format!("{e:#}"), "a handed-off session's budget stayed with it");
                notes.push("Its budget couldn't be moved; it stays with it.".into());
            }
        }
        notes
    }

    /// `t`, assigned to the session a work left, is `to`'s: where it stands
    /// on the board stays.
    async fn ticket_follows(
        &self,
        mut t: crate::board::Ticket,
        to: &SessionView,
        by: &By,
    ) -> Result<(), DeskError> {
        let party = Party::session(to);
        let link = crate::board::Link::Session { session: to.id };
        for _ in 0..3 {
            t.assignee = Some(party.clone());
            t.updated_at = Utc::now();
            match self.save_ticket(&mut t).await {
                Ok(()) => {
                    self.0
                        .store
                        .put_link(t.id, &link, t.updated_at)
                        .await
                        .map_err(DeskError::Internal)?;
                    if !t.links.contains(&link) {
                        t.links.push(link.clone());
                    }
                    let key = self.key_of(&t).await?;
                    let text = format!("{key} follows its work to {}", to.label());
                    self.ticket_changed(t, by, Kind::TicketAssigned, Some(party), text)
                        .await?;
                    return Ok(());
                }
                Err(DeskError::Conflict(_)) => {
                    t = match self.0.store.ticket(t.id).await {
                        Ok(Some(now)) => now,
                        _ => return Ok(()),
                    };
                }
                Err(e) => return Err(e),
            }
        }
        Err(DeskError::Conflict("it kept changing meanwhile".into()))
    }

    /// What becomes of the session the work left, as `after` says; what
    /// the person should know when it couldn't be done.
    fn source_after(&self, id: Uuid, after: After, by: &By) -> Option<String> {
        let live = self.0.state.lock().live.contains_key(&id);
        if !live {
            return None;
        }
        match after {
            After::Keep => {
                Some("It's still running too, so two engines may change the same files.".into())
            }
            After::Stop => match self.stop(id, Some(by.clone())) {
                Ok(_) => None,
                Err(e) => Some(format!("It couldn't be stopped ({e}); it's still running.")),
            },
            After::AfterTurn => {
                let (desk, by) = (self.clone(), by.clone());
                tokio::spawn(async move {
                    let ended = desk
                        .wait_for(
                            id,
                            &[
                                SessionState::Done,
                                SessionState::Idle,
                                SessionState::Interrupted,
                            ],
                            AFTER_TURN_WAIT,
                        )
                        .await;
                    if ended.is_ok_and(|v| {
                        v.live && !matches!(v.state, SessionState::Working | SessionState::Blocked)
                    }) {
                        if let Err(e) = desk.stop(id, Some(by)) {
                            tracing::info!(session = %id, error = %e, "a handed-off session wasn't stopped after its turn");
                        }
                    }
                });
                None
            }
        }
    }

    /// Types `brief` into `id` once it is idle: a target without Desk's
    /// session tools has no other way to read it.
    fn paste_brief(&self, id: Uuid, brief: String) {
        let desk = self.clone();
        tokio::spawn(async move {
            let ready = desk
                .wait_for(id, &[SessionState::Idle, SessionState::Done], PASTE_WAIT)
                .await;
            if !ready.is_ok_and(|v| v.live) {
                tracing::info!(session = %id, "a handed-off session wasn't ready for its brief");
                return;
            }
            let text = brief::rewrap(&brief, &nonce());
            if let Err(e) = desk.type_prompt(id, &text, None) {
                tracing::info!(session = %id, error = %e, "a handed-off session wasn't given its brief");
            }
        });
    }

    /// `brief`, for the session `caller`: the brief it was handed its work
    /// with, wrapped afresh. Only within the group it was handed over in.
    pub(super) async fn brief_call(&self, caller: Uuid) -> Result<Value, String> {
        if self.0.opts.memory.machine {
            return Err(crate::memory::MACHINE_HANDOFF.into());
        }
        let reader = self.session_reader(caller).await?;
        let Reader::Session { group, .. } = &reader else {
            return Err("Desk doesn't know this session.".into());
        };
        let kept = self.0.store.lineage_brief(caller).await.map_err(|e| {
            tracing::warn!(session = %caller, error = format!("{e:#}"), "a session's brief couldn't be read");
            "Desk can't be reached right now; carry on without it.".to_string()
        })?;
        let Some((text, given_in)) = kept else {
            return Ok(json!({
                "brief": null,
                "note": "No work was handed to you from another session, so Desk holds no brief for you. Use recall to look for earlier work.",
            }));
        };
        let Some(text) = text else {
            return Ok(json!({ "brief": null, "note": "The brief for this work was forgotten." }));
        };
        if &given_in != group {
            tracing::info!(session = %caller, "a session asked for a brief given in another group; refused");
            return Err(
                "This brief was given while you were in another group, so it can't be read here."
                    .into(),
            );
        }
        if let Err(e) = self
            .0
            .store
            .memory_read(&reader.label(), "brief", None, &[], group.as_deref())
            .await
        {
            tracing::warn!(session = %caller, error = format!("{e:#}"), "couldn't keep a read of a brief");
        }
        Ok(json!({ "brief": brief::rewrap(&text, &nonce()) }))
    }
}

/// A handoff checked: its source, target, facts and what goes.
struct Checked {
    r: SessionRecord,
    target: Engine,
    facts: Facts,
    content: Content,
    /// The session a hand back resumes.
    back: Option<SessionRecord>,
    /// As asked, with what becomes of the source said.
    ask: Ask,
    /// A hand back's: when this session was handed the work.
    since: Option<DateTime<Utc>>,
}

/// What the person calls `r`.
fn named(r: &SessionRecord) -> &str {
    r.display_name.as_deref().unwrap_or(&r.name)
}

/// A fresh nonce for one brief's wrapping.
fn nonce() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}
