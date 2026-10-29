//! The crew at a glance (`GET /api/desk/overview`): every session and agent
//! in the overview's words, the counts Floor heads its groups with, and the
//! overseer's latest brief.

use chrono::{DateTime, Duration, Utc};
use eyre::Result;
use serde_json::Value;

use crate::crew::{
    self, Brief, Counts, Crew, CrewAgent, Event, Item, ItemApproval, ItemKind, ItemQuestion,
    ItemState, Kind, Overview, Who,
};
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
    let mut state = crew::session_state(v.state);
    if state == ItemState::Working && is_quiet(v.updated_at, now, quiet_minutes) {
        state = ItemState::Quiet;
    }
    if state == ItemState::Done && v.updated_at < midnight {
        state = ItemState::Idle;
    }
    let task = Some(v.goal.trim())
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .or_else(|| v.last_prompt.as_deref().map(|p| crew::one_line(p, 120)));
    let detail = match v.state {
        SessionState::Done | SessionState::Idle => {
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
        question,
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
            needs_you: items
                .iter()
                .filter(|i| i.state == ItemState::NeedsYou)
                .count(),
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

    #[test]
    fn the_brief_is_the_first_paragraph() {
        assert_eq!(
            first_paragraph("\n\nTwo need you.\nMore.\n\nDetails"),
            "Two need you.\nMore."
        );
    }
}
