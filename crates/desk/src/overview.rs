//! The crew at a glance (`GET /api/desk/overview`): every session and agent
//! in the overview's words, the counts Floor heads its groups with, and the
//! overseer's latest brief.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use eyre::Result;
use serde_json::Value;

use crate::crew::{
    self, Brief, Counts, Crew, CrewAgent, Event, Item, ItemApproval, ItemKind, ItemQuestion,
    ItemState, Kind, Overview, PartyKind, Who,
};
use crate::floor::Need;
use crate::model::{SessionState, SessionView};
use crate::service::Desk;

/// The quiet threshold without an overseer to say otherwise.
pub const DEFAULT_QUIET_MINUTES: u32 = 30;

/// Whether work last seen at `last` has gone quiet by `now`: never when the
/// threshold is 0.
pub fn is_quiet(last: DateTime<Utc>, now: DateTime<Utc>, quiet_minutes: u32) -> bool {
    quiet_minutes > 0 && now - last > Duration::minutes(i64::from(quiet_minutes))
}

/// The first paragraph of `text`.
pub fn first_paragraph(text: &str) -> String {
    text.split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn session_item(
    v: &SessionView,
    since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    midnight: DateTime<Utc>,
    quiet_minutes: u32,
) -> Item {
    let waiting = v.waiting_on() > 0;
    let mut state = match waiting {
        true => ItemState::Working,
        false => crew::session_state(v.state),
    };
    if state == ItemState::Working && !waiting && is_quiet(v.updated_at, now, quiet_minutes) {
        state = ItemState::Quiet;
    }
    if state == ItemState::Done && v.updated_at < midnight {
        state = ItemState::Idle;
    }
    let task = Some(v.goal.trim())
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .or_else(|| v.last_prompt.as_deref().map(|p| crew::one_line(p, 120)));
    let detail = match (v.state, v.background) {
        (SessionState::Done, Some(b)) if waiting => Some(match b.count {
            1 => "Waiting on 1 background task".to_string(),
            n => format!("Waiting on {n} background tasks"),
        }),
        (SessionState::Done | SessionState::Idle, _) => {
            v.last_reply.as_deref().map(|r| crew::one_line(r, 140))
        }
        _ => v.current_tool.as_ref().map(|t| format!("● {t}")),
    };
    let question = (v.state == SessionState::Done)
        .then_some(v.last_reply.as_deref())
        .flatten()
        .and_then(|reply| {
            let text = crew::question_in(reply)?;
            let options = crew::options_in(reply, &text);
            Some(ItemQuestion { text, options })
        });
    Item {
        kind: ItemKind::Session,
        id: v.id.to_string(),
        name: v.label().to_string(),
        hub_name: v.display_name.as_ref().map(|_| v.name.clone()),
        cwd: Some(v.cwd.clone()),
        group: v.group.as_ref().map(|g| g.name.clone()),
        engine: Some(v.engine),
        origin: Some(v.origin),
        state,
        task,
        detail,
        since: v
            .pending
            .as_ref()
            .map(|p| p.since)
            .or(since)
            .or(Some(v.updated_at)),
        last_activity: Some(v.updated_at),
        approval: v.pending.as_ref().map(|p| ItemApproval {
            id: p.id.to_string(),
            summary: crew::approval_summary(&p.tool, &p.detail),
        }),
        question: question.filter(|_| !waiting),
        background: v.background.filter(|_| waiting),
    }
}

fn text_of(detail: Option<&Value>, key: &str) -> Option<String> {
    detail?
        .get(key)?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn agent_item(
    a: &CrewAgent,
    latest: Option<&Event>,
    now: DateTime<Utc>,
    midnight: DateTime<Utc>,
    quiet_minutes: u32,
) -> Item {
    let mut item = Item {
        kind: ItemKind::Agent,
        id: a.id.clone(),
        name: a.name.clone(),
        hub_name: None,
        cwd: None,
        group: None,
        engine: None,
        origin: None,
        state: ItemState::Idle,
        task: None,
        detail: None,
        since: None,
        last_activity: None,
        approval: None,
        question: None,
        background: None,
    };
    if let Some(live) = &a.live {
        item.state = if is_quiet(live.last_activity, now, quiet_minutes) {
            ItemState::Quiet
        } else {
            ItemState::Working
        };
        item.task = live.task.clone();
        item.detail = live.detail.clone();
        item.since = Some(live.since);
        item.last_activity = Some(live.last_activity);
        return item;
    }
    let Some(e) = latest else { return item };
    let detail = e.detail.as_ref();
    item.task = text_of(detail, "task");
    item.since = Some(e.at);
    item.last_activity = Some(e.at);
    match e.kind {
        Kind::Question => {
            item.state = ItemState::NeedsYou;
            item.detail = text_of(detail, "answer");
            item.question = detail
                .and_then(|d| d.get("question"))
                .and_then(|q| serde_json::from_value(q.clone()).ok());
        }
        Kind::Finished if e.at >= midnight => {
            item.state = ItemState::Done;
            item.detail = text_of(detail, "answer");
        }
        Kind::Failed | Kind::Limit if e.at >= midnight => {
            item.state = ItemState::Failed;
            item.detail = Some(e.text.clone());
        }
        _ => {}
    }
    item
}

fn rank(state: ItemState) -> u8 {
    match state {
        ItemState::NeedsYou => 0,
        ItemState::Working | ItemState::Quiet => 1,
        ItemState::Done => 2,
        ItemState::Failed => 3,
        ItemState::Idle => 4,
        ItemState::Stopped => 5,
    }
}

/// The overview's party kind for an item.
fn party_of(kind: ItemKind) -> PartyKind {
    match kind {
        ItemKind::Session => PartyKind::Session,
        ItemKind::Agent => PartyKind::Agent,
    }
}

/// `item` as the floor's needs say: it needs you iff one of them is its;
/// one its own record alone said needed you is working (a session) or done
/// with its turn (an agent).
fn as_needed(mut item: Item, needs: &[Need], midnight: DateTime<Utc>) -> Item {
    let needed = needs
        .iter()
        .any(|n| n.who.kind == party_of(item.kind) && n.who.id == item.id);
    item.state = match (needed, item.state, item.kind) {
        (true, _, _) => ItemState::NeedsYou,
        (false, ItemState::NeedsYou, ItemKind::Session) => ItemState::Working,
        (false, ItemState::NeedsYou, ItemKind::Agent) => match item.since {
            Some(at) if at >= midnight => ItemState::Done,
            _ => ItemState::Idle,
        },
        (false, state, _) => state,
    };
    item
}

/// The requests `needs` wait on.
fn waiting_on(needs: &[Need]) -> impl Iterator<Item = &str> {
    needs.iter().filter_map(|n| {
        n.id.strip_prefix("wait:")?
            .split_once(':')
            .map(|(_, key)| key)
    })
}

/// The "Needs you" events that are open needs, newest first: each request
/// still waiting, and the latest question of each subject that still asks.
/// `events` are oldest first.
fn open_events(events: Vec<Event>, needs: &[Need]) -> Vec<Event> {
    let waiting: HashSet<&str> = waiting_on(needs).collect();
    let asking: HashSet<(PartyKind, &str)> = needs
        .iter()
        .filter(|n| n.id.starts_with("question:"))
        .map(|n| (n.who.kind, n.who.id.as_str()))
        .collect();
    let latest_questions: HashMap<(PartyKind, String), String> = events
        .iter()
        .filter(|e| e.kind == Kind::Question)
        .filter_map(|e| {
            let s = e.subject.as_ref()?;
            asking
                .contains(&(s.kind, s.id.as_str()))
                .then(|| ((s.kind, s.id.clone()), e.id.clone()))
        })
        .collect();
    let mut open: Vec<Event> = events
        .into_iter()
        .filter(|e| match e.kind {
            Kind::NeedsYou => e
                .approval_id
                .as_deref()
                .is_some_and(|id| waiting.contains(id)),
            Kind::Question => latest_questions.values().any(|id| *id == e.id),
            _ => false,
        })
        .collect();
    open.reverse();
    open
}

impl Desk {
    /// A page of Activity: up to `limit` events of `filter`, newest first,
    /// from before the event `before`.
    pub async fn activity(
        &self,
        filter: crew::Filter,
        before: Option<&str>,
        limit: i64,
    ) -> Result<crew::ActivityPage> {
        let (events, more) = self.store().events(filter, before, limit).await?;
        Ok(crew::ActivityPage { events, more })
    }

    /// A page of "Needs you": only what the crew's floor says is still open,
    /// where it keeps one; else the log's own.
    pub async fn needs_you(
        &self,
        crew: &dyn Crew,
        before: Option<&str>,
        limit: i64,
    ) -> Result<crew::ActivityPage> {
        let Some(needs) = crew.open_needs().await else {
            return self.activity(crew::Filter::NeedsYou, before, limit).await;
        };
        if needs.is_empty() {
            return Ok(crew::ActivityPage {
                events: vec![],
                more: false,
            });
        }
        let ids: Vec<String> = needs.iter().map(|n| n.who.id.clone()).collect();
        let approvals: Vec<String> = waiting_on(&needs).map(str::to_string).collect();
        let events = self.store().need_events(&ids, &approvals).await?;
        let cursor = before.map(|b| {
            events
                .iter()
                .find(|e| e.id == b)
                .map(|e| (e.at, e.id.clone()))
        });
        let rest: Vec<Event> = open_events(events, &needs)
            .into_iter()
            .filter(|e| match &cursor {
                None => true,
                Some(None) => false,
                Some(Some(c)) => (e.at, e.id.clone()) < *c,
            })
            .collect();
        let more = rest.len() as i64 > limit;
        Ok(crew::ActivityPage {
            events: rest.into_iter().take(limit as usize).collect(),
            more,
        })
    }

    /// The overview as of now.
    pub async fn overview(&self, crew: &dyn Crew) -> Result<Overview> {
        let now = Utc::now();
        let overseer = crew.overseer();
        let quiet = overseer
            .as_ref()
            .map_or(DEFAULT_QUIET_MINUTES, |o| o.quiet_minutes);
        let midnight = crew.midnight();
        let mut items: Vec<Item> = self
            .list()
            .iter()
            .map(|v| session_item(v, self.state_since(v.id), now, midnight, quiet))
            .collect();
        let latest = self.store().latest_turn_events().await?;
        let layout = self.layout();
        items.extend(crew.agents().iter().map(|a| {
            let mut item = agent_item(a, latest.get(&a.id), now, midnight, quiet);
            item.group = layout
                .group_of(&crate::service::local_agent_item(&a.id))
                .map(|g| g.name.clone());
            item
        }));
        let needs = crew.open_needs().await;
        if let Some(needs) = &needs {
            items = items
                .into_iter()
                .map(|i| as_needed(i, needs, midnight))
                .collect();
        }
        items.sort_by(|a, b| {
            rank(a.state)
                .cmp(&rank(b.state))
                .then(b.last_activity.cmp(&a.last_activity))
        });
        let counts = Counts {
            working: items
                .iter()
                .filter(|i| matches!(i.state, ItemState::Working | ItemState::Quiet))
                .count(),
            needs_you: needs.as_ref().map_or_else(
                || {
                    items
                        .iter()
                        .filter(|i| i.state == ItemState::NeedsYou)
                        .count()
                },
                Vec::len,
            ),
            done_today: items.iter().filter(|i| i.state == ItemState::Done).count(),
        };
        let brief = match &overseer {
            None => None,
            Some(o) => self.store().latest_check_in(&o.id).await?.and_then(|c| {
                let text = first_paragraph(c.reply.as_deref()?);
                (!text.is_empty()).then(|| Brief {
                    text,
                    at: c.at,
                    by: Who {
                        id: o.id.clone(),
                        name: o.name.clone(),
                    },
                })
            }),
        };
        Ok(Overview {
            brief,
            counts,
            items,
            groups: layout.groups.iter().map(|g| g.name.clone()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::{LiveTurn, Party};

    #[test]
    fn quiet_is_working_without_a_sign_of_life_past_the_threshold() {
        let now = Utc::now();
        assert!(is_quiet(now - Duration::minutes(31), now, 30));
        assert!(!is_quiet(now - Duration::minutes(29), now, 30));
        assert!(!is_quiet(now - Duration::days(3), now, 0), "0 is never");
    }

    #[test]
    fn an_agent_is_working_asking_done_or_idle_from_its_turns() {
        let now = Utc::now();
        let midnight = now - Duration::hours(2);
        let scout = CrewAgent {
            id: "scout".into(),
            name: "Scout".into(),
            live: None,
        };
        let me = Party::agent("scout", "Scout");
        let asked = Event::new(Kind::Question, me.clone(), Some(me.clone()), "asked").detail(
            serde_json::json!({
                "task": "Staging deploy",
                "question": {"text": "main or release/4.2?", "options": ["main", "release/4.2"]}
            }),
        );
        let item = agent_item(&scout, Some(&asked), now, midnight, 30);
        assert_eq!(item.state, ItemState::NeedsYou);
        assert_eq!(item.task.as_deref(), Some("Staging deploy"));
        assert_eq!(
            item.question.unwrap().options.unwrap(),
            ["main", "release/4.2"]
        );

        let mut done = Event::new(Kind::Finished, me.clone(), Some(me.clone()), "finished");
        assert_eq!(
            agent_item(&scout, Some(&done), now, midnight, 30).state,
            ItemState::Done
        );
        done.at = midnight - Duration::minutes(1);
        assert_eq!(
            agent_item(&scout, Some(&done), now, midnight, 30).state,
            ItemState::Idle
        );
        assert_eq!(
            agent_item(&scout, None, now, midnight, 30).state,
            ItemState::Idle
        );

        let busy = CrewAgent {
            live: Some(LiveTurn {
                task: Some("Compare spend".into()),
                detail: Some("● aws.cost_explorer".into()),
                since: now - Duration::minutes(50),
                last_activity: now - Duration::minutes(40),
            }),
            ..scout
        };
        let item = agent_item(&busy, Some(&done), now, midnight, 30);
        assert_eq!(
            (item.state, item.detail.as_deref()),
            (ItemState::Quiet, Some("● aws.cost_explorer"))
        );
        assert_eq!(
            agent_item(&busy, None, now, midnight, 60).state,
            ItemState::Working
        );
    }

    fn need(id: &str, kind: crate::floor::NeedKind, who: PartyKind, of: &str) -> Need {
        use crate::floor::{Answer, Place, Who};
        Need {
            id: id.into(),
            kind,
            who: Who {
                kind: who,
                id: of.into(),
                name: of.into(),
                engine: None,
            },
            ask: "Ship it?".into(),
            options: None,
            since: Utc::now() - Duration::minutes(5),
            place: Place::Dm,
            answer: Answer::Reply,
        }
    }

    fn item(kind: ItemKind, id: &str, state: ItemState, since: DateTime<Utc>) -> Item {
        Item {
            kind,
            id: id.into(),
            name: id.into(),
            hub_name: None,
            cwd: None,
            group: None,
            engine: None,
            origin: None,
            state,
            task: None,
            detail: None,
            since: Some(since),
            last_activity: Some(since),
            approval: None,
            question: None,
            background: None,
        }
    }

    #[test]
    fn an_item_needs_you_exactly_when_the_floor_says_so() {
        use crate::floor::NeedKind;
        let now = Utc::now();
        let midnight = now - Duration::hours(3);
        let needs = [
            need("question:t1", NeedKind::Question, PartyKind::Session, "s1"),
            need("question:t2", NeedKind::Question, PartyKind::Agent, "scout"),
        ];
        let asked = item(ItemKind::Session, "s1", ItemState::Done, now);
        assert_eq!(
            as_needed(asked, &needs, midnight).state,
            ItemState::NeedsYou,
            "a done session that asked"
        );
        let settled = item(ItemKind::Agent, "lens", ItemState::NeedsYou, now);
        assert_eq!(
            as_needed(settled, &needs, midnight).state,
            ItemState::Done,
            "a question the log settled is done with today"
        );
        let old = item(
            ItemKind::Agent,
            "lens",
            ItemState::NeedsYou,
            midnight - Duration::hours(1),
        );
        assert_eq!(as_needed(old, &needs, midnight).state, ItemState::Idle);
        let answered = item(ItemKind::Session, "s2", ItemState::NeedsYou, now);
        assert_eq!(
            as_needed(answered, &needs, midnight).state,
            ItemState::Working
        );
        let busy = item(ItemKind::Agent, "s1", ItemState::Working, now);
        assert_eq!(
            as_needed(busy, &needs, midnight).state,
            ItemState::Working,
            "another kind's need is not its"
        );
    }

    #[test]
    fn needs_you_lists_only_the_events_still_open() {
        use crate::floor::NeedKind;
        let session = Party {
            kind: PartyKind::Session,
            id: "s1".into(),
            name: "auth-fix".into(),
            engine: None,
            origin: None,
        };
        let scout = Party::agent("scout", "Scout");
        let own = |p: &Party, kind, text: &str| Event::new(kind, p.clone(), Some(p.clone()), text);
        let open_ask = own(&session, Kind::NeedsYou, "wants to run ls").approval("a2");
        let events = vec![
            own(&session, Kind::NeedsYou, "wants to run rm").approval("a1"),
            own(&scout, Kind::Question, "asked “A?”"),
            open_ask.clone(),
            own(&scout, Kind::Question, "asked “B?”"),
            own(&scout, Kind::Finished, "finished"),
        ];
        let latest = events[3].clone();
        let needs = [
            need("wait:t1:a2", NeedKind::Approval, PartyKind::Session, "s1"),
            need("question:t2", NeedKind::Question, PartyKind::Agent, "scout"),
        ];
        assert_eq!(
            open_events(events, &needs)
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            [latest.id, open_ask.id]
        );
    }

    #[test]
    fn the_brief_is_the_first_paragraph() {
        assert_eq!(
            first_paragraph("\n\nTwo need you.\nMore.\n\nDetails"),
            "Two need you.\nMore."
        );
    }
}
